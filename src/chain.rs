//! Signed, hash-linked message log per conversation (issue #6).
//!
//! Every text message between two verified peers is a [`Block`]: signed by
//! its author's ed25519 key and linked to its parent block hashes. Nobody but
//! the two participants can add a block, and no block can be altered or
//! re-parented without breaking every hash after it.
//!
//! The log is a DAG, not a strict chain: when both sides write while
//! disconnected the history forks, and the next block written lists every
//! current head as a parent, merging the branches. Both sides linearise the
//! DAG with the same deterministic topological sort, so they agree on the
//! final order.
//!
//! The root is a virtual genesis hash derived from both public keys. The
//! [`safety_code`] shown by `:verify` is derived from it: if both people see
//! the same code, each is talking to the key the other actually holds.
//!
//! Storage: `<root>/chains/<peer_pubkey_hex>.jsonl`, one block per line.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::identity::{self, Identity};

/// Capability advertised in `hello` by peers that speak this protocol.
pub const CAP: &str = "chain1";

/// Blocks waiting for missing parents, per conversation.
const MAX_ORPHANS: usize = 1024;
/// Largest message body accepted in a block.
pub const MAX_BODY: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// Parent hashes (hex), sorted. The genesis hash for the first block.
    pub prev: Vec<String>,
    /// Author's ed25519 public key (hex).
    pub author: String,
    /// Author's clock, milliseconds since the Unix epoch.
    pub at: i64,
    pub body: String,
    /// Signature (hex) over [`signing_bytes`].
    pub sig: String,
    /// sha256(signing_bytes || sig), hex.
    pub hash: String,
}

/// The conversation root for two public keys (order-independent).
pub fn genesis(pk_a: &[u8; 32], pk_b: &[u8; 32]) -> [u8; 32] {
    let (lo, hi) = if pk_a <= pk_b {
        (pk_a, pk_b)
    } else {
        (pk_b, pk_a)
    };
    let mut h = Sha256::new();
    h.update(b"cweldrop-genesis-v1");
    h.update(lo);
    h.update(hi);
    h.finalize().into()
}

/// A 20-digit code both people can read aloud to confirm they hold each
/// other's real keys, e.g. `"04217 99310 58821 00467"`.
pub fn safety_code(pk_a: &[u8; 32], pk_b: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(b"cweldrop-safety-v1");
    h.update(genesis(pk_a, pk_b));
    let d = h.finalize();
    d.chunks(4)
        .take(4)
        .map(|c| {
            let n = u32::from_be_bytes([c[0], c[1], c[2], c[3]]) % 100_000;
            format!("{n:05}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn signing_bytes(
    genesis: &[u8; 32],
    prev: &[[u8; 32]],
    author: &[u8; 32],
    at: i64,
    body: &str,
) -> Vec<u8> {
    let mut v = Vec::with_capacity(64 + prev.len() * 32 + 48 + body.len());
    v.extend_from_slice(b"cweldrop-block-v1");
    v.extend_from_slice(genesis);
    v.extend_from_slice(&(prev.len() as u32).to_be_bytes());
    for p in prev {
        v.extend_from_slice(p);
    }
    v.extend_from_slice(author);
    v.extend_from_slice(&at.to_be_bytes());
    v.extend_from_slice(&(body.len() as u64).to_be_bytes());
    v.extend_from_slice(body.as_bytes());
    v
}

fn block_hash(signing: &[u8], sig: &[u8; 64]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(signing);
    h.update(sig);
    h.finalize().into()
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    hex::decode(s).ok()?.try_into().ok()
}

fn hex64(s: &str) -> Option<[u8; 64]> {
    hex::decode(s).ok()?.try_into().ok()
}

/// One entry of a linearised conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub hash: String,
    pub author_is_me: bool,
    pub at: i64,
    pub body: String,
    /// For my blocks: written to the peer, or covered by heads it announced.
    pub delivered: bool,
    /// For my blocks: the peer has signed a block descending from it, so it
    /// provably reached them.
    pub acked: bool,
}

/// The log of one conversation.
pub struct Chain {
    me: [u8; 32],
    peer: [u8; 32],
    genesis: [u8; 32],
    genesis_hex: String,
    blocks: HashMap<String, Block>,
    heads: BTreeSet<String>,
    orphans: Vec<Block>,
    path: Option<PathBuf>,
    /// Hashes known to have reached the peer; persisted next to the chain.
    sent: HashSet<String>,
}

/// What [`Chain::insert`] did with a block.
#[derive(Debug, PartialEq, Eq)]
pub enum Inserted {
    /// Stored, along with any previously orphaned blocks it unblocked; all of
    /// their hashes, in insertion order.
    New(Vec<String>),
    Duplicate,
    /// Parents not known yet; held until they arrive.
    Orphaned,
}

impl Chain {
    pub fn new(me: [u8; 32], peer: [u8; 32], path: Option<PathBuf>) -> Self {
        let g = genesis(&me, &peer);
        Self {
            me,
            peer,
            genesis: g,
            genesis_hex: hex::encode(g),
            blocks: HashMap::new(),
            heads: BTreeSet::new(),
            orphans: Vec::new(),
            path,
            sent: HashSet::new(),
        }
    }

    /// Load from `path`, re-verifying every block. Bad lines are skipped.
    pub fn load(me: [u8; 32], peer: [u8; 32], path: PathBuf) -> Result<Self> {
        let mut c = Self::new(me, peer, None);
        if path.exists() {
            let f = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
            for line in BufReader::new(f).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Block>(&line) {
                    Ok(b) => {
                        if let Err(e) = c.insert(b) {
                            tracing::warn!(error = %e, "skip invalid stored block");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "skip malformed chain line"),
                }
            }
        }
        if let Ok(sent) = fs::read_to_string(path.with_extension("sent")) {
            c.sent = sent
                .lines()
                .map(str::trim)
                .filter(|h| c.blocks.contains_key(*h))
                .map(String::from)
                .collect();
        }
        c.path = Some(path);
        Ok(c)
    }

    /// Record that the peer has these blocks (written to it, or announced in
    /// its heads). Returns the hashes that weren't already recorded.
    pub fn mark_sent<'a>(&mut self, hashes: impl IntoIterator<Item = &'a String>) -> Vec<String> {
        let new: Vec<String> = hashes
            .into_iter()
            .filter(|h| self.blocks.contains_key(*h) && !self.sent.contains(*h))
            .cloned()
            .collect();
        if new.is_empty() {
            return new;
        }
        self.sent.extend(new.iter().cloned());
        if let Some(path) = &self.path {
            let res = (|| -> std::io::Result<()> {
                let mut f = append_opts().open(path.with_extension("sent"))?;
                f.write_all(new.join("\n").as_bytes())?;
                f.write_all(b"\n")
            })();
            if let Err(e) = res {
                tracing::warn!(error = %e, "persisting delivery state failed");
            }
        }
        new
    }

    /// Everything the peer provably has, given the heads it announced.
    pub fn mark_peer_heads(&mut self, their_heads: &[String]) -> Vec<String> {
        let have: Vec<String> = self.ancestors(their_heads).into_iter().collect();
        self.mark_sent(&have)
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn contains(&self, hash: &str) -> bool {
        self.blocks.contains_key(hash)
    }

    pub fn get(&self, hash: &str) -> Option<&Block> {
        self.blocks.get(hash)
    }

    pub fn heads(&self) -> Vec<String> {
        self.heads.iter().cloned().collect()
    }

    /// Parents for the next block: all heads (merging any fork), or genesis.
    fn next_prev(&self) -> Vec<String> {
        if self.heads.is_empty() {
            vec![self.genesis_hex.clone()]
        } else {
            self.heads()
        }
    }

    /// Author a new block as `me`.
    pub fn compose(&mut self, identity: &Identity, body: &str, at: i64) -> Result<Block> {
        if identity.pubkey_bytes() != self.me {
            bail!("identity does not own this chain");
        }
        let prev = self.next_prev();
        let prev_raw: Vec<[u8; 32]> = prev.iter().filter_map(|p| hex32(p)).collect();
        let signing = signing_bytes(&self.genesis, &prev_raw, &self.me, at, body);
        let sig = identity.sign(&signing).to_bytes();
        let block = Block {
            prev,
            author: hex::encode(self.me),
            at,
            body: body.to_string(),
            sig: hex::encode(sig),
            hash: hex::encode(block_hash(&signing, &sig)),
        };
        match self.insert(block.clone())? {
            Inserted::New(_) => Ok(block),
            other => Err(anyhow!("own block not stored: {other:?}")),
        }
    }

    /// Check hash, signature and authorship (not parent presence).
    fn verify(&self, b: &Block) -> Result<()> {
        let author = hex32(&b.author).ok_or_else(|| anyhow!("bad author key"))?;
        if author != self.me && author != self.peer {
            bail!("author is not part of this conversation");
        }
        if b.body.len() > MAX_BODY {
            bail!("body too large");
        }
        if b.prev.is_empty() || b.prev.windows(2).any(|w| w[0] >= w[1]) {
            bail!("parents must be non-empty, sorted and unique");
        }
        let prev_raw = b
            .prev
            .iter()
            .map(|p| hex32(p).ok_or_else(|| anyhow!("bad parent hash")))
            .collect::<Result<Vec<_>>>()?;
        let sig = hex64(&b.sig).ok_or_else(|| anyhow!("bad signature encoding"))?;
        let signing = signing_bytes(&self.genesis, &prev_raw, &author, b.at, &b.body);
        if hex::encode(block_hash(&signing, &sig)) != b.hash {
            bail!("hash mismatch");
        }
        if !identity::verify(&author, &signing, &sig) {
            bail!("signature did not verify");
        }
        Ok(())
    }

    fn parents_known(&self, b: &Block) -> bool {
        b.prev
            .iter()
            .all(|p| *p == self.genesis_hex || self.blocks.contains_key(p))
    }

    /// Verify and add a block. Errors mean the block is forged or corrupt.
    pub fn insert(&mut self, b: Block) -> Result<Inserted> {
        if self.blocks.contains_key(&b.hash) {
            return Ok(Inserted::Duplicate);
        }
        self.verify(&b)?;
        if !self.parents_known(&b) {
            if self.orphans.iter().all(|o| o.hash != b.hash) {
                if self.orphans.len() >= MAX_ORPHANS {
                    self.orphans.remove(0);
                }
                self.orphans.push(b);
            }
            return Ok(Inserted::Orphaned);
        }
        let mut added = Vec::new();
        self.store(b, &mut added)?;
        // Adopt any orphans this unblocked, repeatedly.
        while let Some(i) = self.orphans.iter().position(|o| self.parents_known(o)) {
            let o = self.orphans.remove(i);
            if !self.blocks.contains_key(&o.hash) {
                self.store(o, &mut added)?;
            }
        }
        Ok(Inserted::New(added))
    }

    fn store(&mut self, b: Block, added: &mut Vec<String>) -> Result<()> {
        if let Some(path) = &self.path {
            append_line(path, &b)?;
        }
        for p in &b.prev {
            self.heads.remove(p);
        }
        self.heads.insert(b.hash.clone());
        added.push(b.hash.clone());
        self.blocks.insert(b.hash.clone(), b);
        Ok(())
    }

    /// `roots` and everything they descend from.
    fn ancestors<'a>(&self, roots: impl IntoIterator<Item = &'a String>) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut stack: Vec<String> = roots
            .into_iter()
            .filter(|h| self.blocks.contains_key(*h))
            .cloned()
            .collect();
        while let Some(h) = stack.pop() {
            if !seen.insert(h.clone()) {
                continue;
            }
            if let Some(b) = self.blocks.get(&h) {
                stack.extend(
                    b.prev
                        .iter()
                        .filter(|p| self.blocks.contains_key(*p))
                        .cloned(),
                );
            }
        }
        seen
    }

    /// Blocks a peer whose heads are `their_heads` may be missing, parents
    /// first. Unknown heads are ignored (they're blocks we lack), so this can
    /// over-send a little; receivers drop duplicates.
    pub fn missing_for(&self, their_heads: &[String]) -> Vec<Block> {
        let have = self.ancestors(their_heads);
        self.linear_hashes()
            .into_iter()
            .filter(|h| !have.contains(h))
            .filter_map(|h| self.blocks.get(&h).cloned())
            .collect()
    }

    /// Deterministic topological order: among blocks whose parents are all
    /// placed, the earliest (at, hash) goes next.
    fn linear_hashes(&self) -> Vec<String> {
        let mut pending: HashMap<&str, usize> = HashMap::new();
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut ready = BinaryHeap::new();
        for (h, b) in &self.blocks {
            let known: Vec<&String> = b
                .prev
                .iter()
                .filter(|p| self.blocks.contains_key(*p))
                .collect();
            pending.insert(h, known.len());
            for p in known {
                children.entry(p).or_default().push(h);
            }
            if pending[h.as_str()] == 0 {
                ready.push(Reverse((b.at, h.as_str())));
            }
        }
        let mut out = Vec::with_capacity(self.blocks.len());
        while let Some(Reverse((_, h))) = ready.pop() {
            out.push(h.to_string());
            for c in children.get(h).into_iter().flatten() {
                let n = pending.get_mut(c).expect("child tracked");
                *n -= 1;
                if *n == 0 {
                    ready.push(Reverse((self.blocks[*c].at, *c)));
                }
            }
        }
        out
    }

    /// The conversation in display order.
    pub fn entries(&self) -> Vec<Entry> {
        let me_hex = hex::encode(self.me);
        let peer_hex = hex::encode(self.peer);
        let peer_heads: Vec<&String> = self
            .blocks
            .values()
            .filter(|b| b.author == peer_hex)
            .map(|b| &b.hash)
            .collect();
        let acked = self.ancestors(peer_heads);
        self.linear_hashes()
            .into_iter()
            .map(|h| {
                let b = &self.blocks[&h];
                let author_is_me = b.author == me_hex;
                Entry {
                    delivered: author_is_me && self.sent.contains(&h),
                    acked: author_is_me && acked.contains(&h),
                    hash: h,
                    author_is_me,
                    at: b.at,
                    body: b.body.clone(),
                }
            })
            .collect()
    }

    pub fn safety_code(&self) -> String {
        safety_code(&self.me, &self.peer)
    }
}

fn append_opts() -> fs::OpenOptions {
    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts
}

fn append_line(path: &Path, b: &Block) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut f = append_opts()
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut line = serde_json::to_vec(b)?;
    line.push(b'\n');
    f.write_all(&line)?;
    Ok(())
}

/// All conversations of the local identity, loaded lazily and shared between
/// the UI and the network tasks.
pub struct Chains {
    identity: Arc<Identity>,
    root: PathBuf,
    open: Mutex<HashMap<String, Chain>>,
    /// Last known display name per peer pubkey (`names.json`), loaded lazily.
    names: Mutex<Option<HashMap<String, String>>>,
}

impl Chains {
    /// Store under `<data_dir>/chains`.
    pub fn open(identity: Arc<Identity>) -> Result<Arc<Self>> {
        Ok(Self::open_in(
            identity,
            identity::data_dir()?.join("chains"),
        ))
    }

    pub fn open_in(identity: Arc<Identity>, root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            identity,
            root,
            open: Mutex::new(HashMap::new()),
            names: Mutex::new(None),
        })
    }

    fn names_path(&self) -> PathBuf {
        self.root.join("names.json")
    }

    fn with_names<T>(&self, f: impl FnOnce(&mut HashMap<String, String>) -> T) -> T {
        let mut guard = self.names.lock().unwrap_or_else(|e| e.into_inner());
        let names = guard.get_or_insert_with(|| {
            fs::read(self.names_path())
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default()
        });
        f(names)
    }

    /// Last name seen for `peer_hex`, for listing offline contacts.
    pub fn name(&self, peer_hex: &str) -> Option<String> {
        self.with_names(|n| n.get(peer_hex).cloned())
    }

    /// Remember `name` for `peer_hex` (written only when it changes).
    pub fn set_name(&self, peer_hex: &str, name: &str) -> Result<()> {
        let snapshot = self.with_names(|n| {
            if n.get(peer_hex).map(String::as_str) == Some(name) {
                None
            } else {
                n.insert(peer_hex.to_string(), name.to_string());
                Some(n.clone())
            }
        });
        let Some(names) = snapshot else {
            return Ok(());
        };
        fs::create_dir_all(&self.root)?;
        let tmp = self.root.join("names.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&names)?)?;
        fs::rename(&tmp, self.names_path())?;
        Ok(())
    }

    pub fn my_pubkey(&self) -> String {
        self.identity.pubkey_hex()
    }

    /// Run `f` on the chain with `peer_hex`, loading it on first use.
    pub fn with<T>(&self, peer_hex: &str, f: impl FnOnce(&mut Chain) -> T) -> Result<T> {
        let peer = hex32(peer_hex).ok_or_else(|| anyhow!("bad peer key"))?;
        let mut open = self
            .open
            .lock()
            .map_err(|_| anyhow!("chain lock poisoned"))?;
        if !open.contains_key(peer_hex) {
            let path = self.root.join(format!("{peer_hex}.jsonl"));
            let chain = Chain::load(self.identity.pubkey_bytes(), peer, path)?;
            open.insert(peer_hex.to_string(), chain);
        }
        Ok(f(open.get_mut(peer_hex).expect("just inserted")))
    }

    /// Write a message to `peer_hex` (online or not).
    pub fn compose(&self, peer_hex: &str, body: &str) -> Result<Block> {
        let at = chrono::Utc::now().timestamp_millis();
        let identity = Arc::clone(&self.identity);
        self.with(peer_hex, |c| c.compose(&identity, body, at))?
    }

    /// Pubkeys of everyone we have a stored conversation with.
    pub fn known_peers(&self) -> Vec<String> {
        let Ok(rd) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        rd.filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let pk = name.strip_suffix(".jsonl")?;
                hex32(pk).map(|_| pk.to_string())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        let dir = std::env::temp_dir().join(format!("cweldrop-chain-id-{}", rand::random::<u64>()));
        let id = Identity::load_or_create_in(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        id
    }

    fn pair() -> (Identity, Identity, Chain, Chain) {
        let (a, b) = (identity(), identity());
        let ca = Chain::new(a.pubkey_bytes(), b.pubkey_bytes(), None);
        let cb = Chain::new(b.pubkey_bytes(), a.pubkey_bytes(), None);
        (a, b, ca, cb)
    }

    fn bodies(c: &Chain) -> Vec<String> {
        c.entries().into_iter().map(|e| e.body).collect()
    }

    /// Exchange blocks both ways like the connect-time sync does.
    fn sync(x: &mut Chain, y: &mut Chain) {
        for b in x.missing_for(&y.heads()) {
            y.insert(b).unwrap();
        }
        for b in y.missing_for(&x.heads()) {
            x.insert(b).unwrap();
        }
    }

    #[test]
    fn genesis_and_safety_code_are_symmetric() {
        let (a, b) = (identity(), identity());
        let (pa, pb) = (a.pubkey_bytes(), b.pubkey_bytes());
        assert_eq!(genesis(&pa, &pb), genesis(&pb, &pa));
        let code = safety_code(&pa, &pb);
        assert_eq!(code, safety_code(&pb, &pa));
        assert_eq!(code.len(), 23);
        let c = identity();
        assert_ne!(code, safety_code(&pa, &c.pubkey_bytes()));
    }

    #[test]
    fn linear_conversation_replicates() {
        let (a, b, mut ca, mut cb) = pair();
        let m1 = ca.compose(&a, "hi", 1).unwrap();
        assert_eq!(
            cb.insert(m1).unwrap(),
            Inserted::New(vec![ca.heads()[0].clone()])
        );
        let m2 = cb.compose(&b, "hey", 2).unwrap();
        ca.insert(m2).unwrap();
        assert_eq!(bodies(&ca), vec!["hi", "hey"]);
        assert_eq!(bodies(&ca), bodies(&cb));
        assert_eq!(ca.heads(), cb.heads());
        // b's reply descends from a's message: a sees it as acknowledged.
        assert!(ca.entries()[0].acked);
        assert!(!cb.entries()[1].acked, "nobody acked b's last message yet");
    }

    #[test]
    fn offline_fork_merges_to_same_order() {
        let (a, b, mut ca, mut cb) = pair();
        let m = ca.compose(&a, "before", 10).unwrap();
        cb.insert(m).unwrap();
        // Disconnected: both write.
        ca.compose(&a, "a offline 1", 20).unwrap();
        ca.compose(&a, "a offline 2", 40).unwrap();
        cb.compose(&b, "b offline", 30).unwrap();
        sync(&mut ca, &mut cb);
        assert_eq!(ca.len(), 4);
        assert_eq!(bodies(&ca), bodies(&cb), "same synced order on both sides");
        assert_eq!(
            bodies(&ca),
            vec!["before", "a offline 1", "b offline", "a offline 2"]
        );
        assert_eq!(ca.heads().len(), 2, "fork until someone writes");
        let merge = cb.compose(&b, "merged", 50).unwrap();
        assert_eq!(merge.prev.len(), 2, "next block merges both branches");
        ca.insert(merge).unwrap();
        assert_eq!(ca.heads(), cb.heads());
        assert_eq!(ca.heads().len(), 1);
        assert!(ca
            .entries()
            .iter()
            .filter(|e| e.author_is_me)
            .all(|e| e.acked));
    }

    #[test]
    fn forged_and_tampered_blocks_rejected() {
        let (a, _b, mut ca, mut cb) = pair();
        let mallory = identity();
        // Outsider signing for this conversation.
        let mut cm = Chain::new(mallory.pubkey_bytes(), ca.peer, None);
        let forged = cm.compose(&mallory, "trust me", 1).unwrap();
        assert!(cb.insert(forged).is_err(), "outsider author");

        let good = ca.compose(&a, "pay 10", 1).unwrap();
        let mut tampered = good.clone();
        tampered.body = "pay 1000".into();
        assert!(cb.insert(tampered).is_err(), "body changed");

        let mut resigned = good.clone();
        resigned.at = 2;
        assert!(cb.insert(resigned).is_err(), "timestamp changed");

        let mut bad_sig = good.clone();
        bad_sig.sig = "00".repeat(64);
        assert!(cb.insert(bad_sig).is_err(), "signature replaced");

        let mut reparented = good.clone();
        reparented.prev = vec!["11".repeat(32)];
        assert!(cb.insert(reparented).is_err(), "parents changed");

        assert!(matches!(cb.insert(good.clone()).unwrap(), Inserted::New(_)));
        assert_eq!(cb.insert(good).unwrap(), Inserted::Duplicate);
    }

    #[test]
    fn out_of_order_blocks_wait_for_parents() {
        let (a, _b, mut ca, mut cb) = pair();
        let m1 = ca.compose(&a, "1", 1).unwrap();
        let m2 = ca.compose(&a, "2", 2).unwrap();
        let m3 = ca.compose(&a, "3", 3).unwrap();
        assert_eq!(cb.insert(m3.clone()).unwrap(), Inserted::Orphaned);
        assert_eq!(cb.insert(m2.clone()).unwrap(), Inserted::Orphaned);
        assert_eq!(
            cb.insert(m1.clone()).unwrap(),
            Inserted::New(vec![m1.hash, m2.hash, m3.hash])
        );
        assert_eq!(bodies(&cb), vec!["1", "2", "3"]);
    }

    #[test]
    fn missing_for_sends_only_what_peer_lacks() {
        let (a, _b, mut ca, mut cb) = pair();
        let m1 = ca.compose(&a, "1", 1).unwrap();
        cb.insert(m1).unwrap();
        ca.compose(&a, "2", 2).unwrap();
        let missing = ca.missing_for(&cb.heads());
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].body, "2");
        assert!(ca.missing_for(&ca.heads()).is_empty());
        assert_eq!(ca.missing_for(&[]).len(), 2, "empty peer gets everything");
    }

    #[test]
    fn persists_and_reloads() {
        let (a, b) = (identity(), identity());
        let path = std::env::temp_dir()
            .join(format!("cweldrop-chain-{}", rand::random::<u64>()))
            .join("peer.jsonl");
        let mut c = Chain::load(a.pubkey_bytes(), b.pubkey_bytes(), path.clone()).unwrap();
        c.compose(&a, "one", 1).unwrap();
        c.compose(&a, "two", 2).unwrap();
        let again = Chain::load(a.pubkey_bytes(), b.pubkey_bytes(), path.clone()).unwrap();
        assert_eq!(bodies(&again), vec!["one", "two"]);
        assert_eq!(again.heads(), c.heads());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn delivery_state_persists() {
        let (a, b) = (identity(), identity());
        let dir = std::env::temp_dir().join(format!("cweldrop-chain-{}", rand::random::<u64>()));
        let path = dir.join("peer.jsonl");
        let mut c = Chain::load(a.pubkey_bytes(), b.pubkey_bytes(), path.clone()).unwrap();
        let m1 = c.compose(&a, "one", 1).unwrap();
        let m2 = c.compose(&a, "two", 2).unwrap();
        assert!(c.entries().iter().all(|e| !e.delivered));
        // Peer announced m1 as its head: m1 is delivered, m2 not yet.
        assert_eq!(
            c.mark_peer_heads(std::slice::from_ref(&m1.hash)),
            vec![m1.hash.clone()]
        );
        assert!(c.mark_sent([&m1.hash]).is_empty(), "already known");
        let again = Chain::load(a.pubkey_bytes(), b.pubkey_bytes(), path).unwrap();
        let d: Vec<bool> = again.entries().iter().map(|e| e.delivered).collect();
        assert_eq!(d, vec![true, false]);
        let _ = m2;
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_persist() {
        let me = Arc::new(identity());
        let root = std::env::temp_dir().join(format!("cweldrop-chains-{}", rand::random::<u64>()));
        let chains = Chains::open_in(Arc::clone(&me), root.clone());
        assert_eq!(chains.name("ab"), None);
        chains.set_name("ab", "bob").unwrap();
        let reopened = Chains::open_in(me, root.clone());
        assert_eq!(reopened.name("ab").as_deref(), Some("bob"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn registry_lists_known_peers() {
        let me = Arc::new(identity());
        let peer = identity().pubkey_hex();
        let root = std::env::temp_dir().join(format!("cweldrop-chains-{}", rand::random::<u64>()));
        let chains = Chains::open_in(Arc::clone(&me), root.clone());
        assert!(chains.known_peers().is_empty());
        chains.compose(&peer, "queued").unwrap();
        assert_eq!(chains.known_peers(), vec![peer.clone()]);
        assert!(chains.with("zz", |_| ()).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
