use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};

use crate::chain::{self, Chains};
use crate::event::AppEvent;
use crate::files;
use crate::history_store;
use crate::net::{NetCmd, PeerId};
use crate::preview::Content;
use crate::settings::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStatus {
    Available, // discovered via mDNS, not yet connected
    Connecting,
    Online,
    Offline,
    Error,
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub id: PeerId,
    pub username: String,
    pub addr: Option<SocketAddr>,
    pub status: PeerStatus,
    pub last_error: Option<String>,
    /// Verified ed25519 pubkey (hex) of the remote, if the handshake completed.
    pub pubkey: Option<String>,
    /// When the remote last signalled it was typing. Cleared on stop or expiry.
    pub typing_at: Option<Instant>,
    /// Speaks the signed message chain; text goes via `crate::chain` and can
    /// be queued while the peer is offline.
    pub chain: bool,
    /// Pubkey announced over mDNS, not yet proven by a handshake.
    pub claimed_pubkey: Option<String>,
    /// mDNS instance id announcing this peer, when the row has another id
    /// (a connection or contact row that absorbed the discovery).
    pub mdns_id: Option<PeerId>,
    /// The user compared safety codes with this key (`:verify confirm`).
    pub verified: bool,
    /// Identity warning, e.g. a new key using a known contact's name.
    pub warning: Option<String>,
}

/// How long a "typing…" indicator stays visible after the last ping. The
/// sender re-pings roughly once a second while editing, so this only lapses
/// once they actually stop.
pub const TYPING_TTL: Duration = Duration::from_secs(5);

impl Peer {
    pub fn new(id: PeerId, username: String, addr: Option<SocketAddr>, status: PeerStatus) -> Self {
        Self {
            id,
            username,
            addr,
            status,
            last_error: None,
            pubkey: None,
            typing_at: None,
            chain: false,
            claimed_pubkey: None,
            mdns_id: None,
            verified: false,
            warning: None,
        }
    }

    /// Short key fingerprint for display: verified key, else the claimed one.
    pub fn fingerprint(&self) -> Option<&str> {
        self.pubkey
            .as_deref()
            .or(self.claimed_pubkey.as_deref())
            .map(|k| &k[..8.min(k.len())])
    }

    /// Key under which chat lines are stored in `App.history` for this peer.
    /// Verified peers use their pubkey so transcripts survive across nicks and
    /// connection ids; unverified peers fall back to the connection id.
    pub fn history_key(&self) -> &str {
        self.pubkey.as_deref().unwrap_or(&self.id)
    }

    /// True if the peer is currently typing (last ping within `TYPING_TTL`).
    pub fn is_typing(&self) -> bool {
        self.typing_at.is_some_and(|t| t.elapsed() < TYPING_TTL)
    }
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    pub from: String,
    pub body: String,
    pub at: SystemTime,
    /// Present when this line is a file transfer rather than a text message.
    pub file: Option<FileInfo>,
    /// Present when this message is a signed chain block.
    pub chain: Option<ChainMark>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainMark {
    pub hash: String,
    pub delivery: Delivery,
}

/// Delivery state of a chain message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Written by the peer.
    Received,
    /// Ours, not yet sent (peer offline): goes out on the next sync.
    Queued,
    /// Ours, written to the peer's socket.
    Sent,
    /// Ours, and the peer signed a later block on top of it: provably seen.
    Acked,
}

impl ChatLine {
    pub fn text(from: String, body: String) -> Self {
        Self {
            from,
            body,
            at: SystemTime::now(),
            file: None,
            chain: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// Transfer id; empty for transfers loaded from history.
    pub file_id: String,
    pub name: String,
    pub size: u64,
    /// Local path: the source for sent files, the saved copy for received.
    pub path: Option<PathBuf>,
    pub state: FileState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    /// We offered it; waiting for the peer to answer.
    Offered,
    /// The peer offered it; waiting for `:accept` / `:reject`.
    AwaitingDecision,
    /// Bytes moved so far.
    Transferring(u64),
    Done,
    Rejected,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Peers,
    Input,
    Command,
    Settings,
    /// The `:files` panel.
    Files,
}

/// Keys of the `:files` panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesAction {
    Up,
    Down,
    /// Accept a pending offer, or view a finished file.
    Open,
    Accept,
    Reject,
    AcceptAll,
    RejectAll,
    Close,
}

/// Which of several candidates a command argument means.
#[derive(Debug, PartialEq, Eq)]
enum Pick {
    One(usize),
    All,
    /// No argument and more than one candidate.
    Ambiguous,
    Nothing,
}

/// Resolve `arg` against `items` (newest first): empty = the only one,
/// `all`, `n` = n-th newest, otherwise an exact or unique partial file name.
fn pick(items: &[FileInfo], arg: &str) -> Pick {
    let arg = arg.trim();
    if items.is_empty() {
        return Pick::Nothing;
    }
    if arg.is_empty() {
        return if items.len() == 1 {
            Pick::One(0)
        } else {
            Pick::Ambiguous
        };
    }
    if arg.eq_ignore_ascii_case("all") {
        return Pick::All;
    }
    if let Ok(n) = arg.parse::<usize>() {
        return if (1..=items.len()).contains(&n) {
            Pick::One(n - 1)
        } else {
            Pick::Nothing
        };
    }
    if let Some(i) = items.iter().position(|f| f.name.eq_ignore_ascii_case(arg)) {
        return Pick::One(i);
    }
    let needle = arg.to_lowercase();
    let hits: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, f)| f.name.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect();
    match hits[..] {
        [i] => Pick::One(i),
        [] => Pick::Nothing,
        _ => Pick::Ambiguous,
    }
}

/// Rows of the settings panel, in display order.
pub const SETTINGS_ROWS: usize = 3;

/// Full-screen file viewer state.
#[derive(Debug)]
pub struct Viewer {
    pub name: String,
    pub path: PathBuf,
    /// `None` while loading.
    pub content: Option<Content>,
    pub error: Option<String>,
    /// First visible line (text only); clamped when drawn.
    pub scroll: u16,
    /// Page shown for paged documents (PDF), 1-based.
    pub page: u32,
}

pub struct App {
    pub self_username: String,
    pub self_port: u16,
    pub self_pubkey: Option<String>,
    pub peers: Vec<Peer>,
    pub history: HashMap<String, Vec<ChatLine>>,
    pub selected: usize,
    pub focus: Focus,
    pub input: String,
    pub input_cursor: usize,
    pub command: String,
    pub command_cursor: usize,
    /// Wrapped-line offset from the bottom of the chat. 0 = stick to newest.
    pub chat_scroll: u16,
    pub status_msg: String,
    pub should_quit: bool,
    /// Whether the terminal window currently has focus (driven by crossterm
    /// focus events). Used to decide when to fire desktop notifications.
    pub terminal_focused: bool,
    /// Throttle: when we last sent a typing ping to the selected peer.
    last_typing_sent: Option<Instant>,
    /// Clickable link cells from the last render (one entry per screen row a
    /// link occupies). Rebuilt by `ui::draw` every frame.
    pub link_regions: Vec<LinkRegion>,
    /// A link the user asked to open; drained and opened by the main loop so
    /// state handling stays free of side effects.
    pub pending_open: Option<String>,
    pub settings: Settings,
    /// Write settings to disk on change. Off in tests.
    pub persist_settings: bool,
    /// Highlighted row in the settings panel.
    pub settings_sel: usize,
    /// In-progress edit of the download folder, if any.
    pub settings_edit: Option<String>,
    /// Highlighted row in the `:files` panel.
    pub files_sel: usize,
    /// Highlighted suggestion in the command hint panel.
    pub cmd_sel: usize,
    /// NetCmds produced while handling events; drained by the main loop.
    pub outbox: Vec<NetCmd>,
    pub viewer: Option<Viewer>,
    /// An image the main loop should decode for the viewer.
    pub pending_view: Option<PathBuf>,
    /// Ask the main loop for a full terminal clear (after graphics overlays).
    pub needs_clear: bool,
    /// Signed message chains; `None` disables chain messaging (tests).
    pub chains: Option<Arc<Chains>>,
}

/// What a clickable region in the chat leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Url(String),
    /// A received or sent file, opened in the viewer.
    File(PathBuf),
}

/// A horizontal run of screen cells that shows (part of) a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRegion {
    pub y: u16,
    pub x_start: u16,
    /// Exclusive.
    pub x_end: u16,
    pub target: LinkTarget,
}

impl App {
    pub fn new(self_username: String, self_port: u16) -> Self {
        Self {
            self_username,
            self_port,
            self_pubkey: None,
            peers: Vec::new(),
            history: HashMap::new(),
            selected: 0,
            focus: Focus::Peers,
            input: String::new(),
            input_cursor: 0,
            command: String::new(),
            command_cursor: 0,
            chat_scroll: 0,
            status_msg: String::new(),
            should_quit: false,
            terminal_focused: true,
            last_typing_sent: None,
            link_regions: Vec::new(),
            pending_open: None,
            settings: Settings::default(),
            persist_settings: false,
            settings_sel: 0,
            settings_edit: None,
            files_sel: 0,
            cmd_sel: 0,
            outbox: Vec::new(),
            viewer: None,
            pending_view: None,
            needs_clear: false,
            chains: None,
        }
    }

    /// Enable chain messaging and list everyone we have a chain with as an
    /// offline contact, so messages can be queued for them right away.
    pub fn with_chains(mut self, chains: Arc<Chains>) -> Self {
        let known = chains.known_peers();
        self.chains = Some(chains);
        for pk in known {
            if self
                .peers
                .iter()
                .any(|p| p.pubkey.as_deref() == Some(pk.as_str()))
            {
                continue;
            }
            self.load_history(&pk);
            let from_history = self
                .history
                .get(&pk)
                .and_then(|h| h.iter().rev().find(|l| l.from != self.self_username))
                .map(|l| l.from.clone());
            let username = self
                .chains
                .as_ref()
                .and_then(|c| c.name(&pk))
                .or(from_history)
                .unwrap_or_else(|| format!("{}…", &pk[..8]));
            let verified = self.chains.as_ref().is_some_and(|c| c.is_verified(&pk));
            self.peers.push(Peer {
                pubkey: Some(pk.clone()),
                chain: true,
                verified,
                ..Peer::new(format!("contact:{pk}"), username, None, PeerStatus::Offline)
            });
            self.rebuild_chain_lines(&pk);
        }
        self
    }

    /// Load the persisted (non-chain) transcript for `pubkey`.
    fn load_history(&mut self, pubkey: &str) {
        match history_store::load(pubkey) {
            Ok(lines) => {
                let loaded: Vec<ChatLine> = lines
                    .into_iter()
                    .map(|sl| ChatLine {
                        from: sl.from,
                        body: sl.body,
                        at: system_time_from_chrono(sl.at),
                        file: sl.file.map(|f| FileInfo {
                            file_id: String::new(),
                            name: f.name,
                            size: f.size,
                            path: Some(f.path),
                            state: FileState::Done,
                        }),
                        chain: None,
                    })
                    .collect();
                self.history.insert(pubkey.to_string(), loaded);
            }
            Err(e) => {
                tracing::warn!(error=%e, %pubkey, "history load failed");
                self.history.entry(pubkey.to_string()).or_default();
            }
        }
    }

    /// Re-derive the chain messages of the conversation with `pubkey` and
    /// merge them, by time, with its other lines (files, legacy text).
    fn rebuild_chain_lines(&mut self, pubkey: &str) {
        let Some(chains) = self.chains.clone() else {
            return;
        };
        let entries = match chains.with(pubkey, |c| c.entries()) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, %pubkey, "chain load failed");
                return;
            }
        };
        if entries.is_empty() {
            return;
        }
        let peer_name = self
            .peers
            .iter()
            .find(|p| p.pubkey.as_deref() == Some(pubkey))
            .map(|p| p.username.clone())
            .unwrap_or_else(|| format!("{}…", &pubkey[..8.min(pubkey.len())]));
        let chain_lines: Vec<ChatLine> = entries
            .into_iter()
            .map(|e| {
                let delivery = if !e.author_is_me {
                    Delivery::Received
                } else if e.acked {
                    Delivery::Acked
                } else if e.delivered {
                    Delivery::Sent
                } else {
                    Delivery::Queued
                };
                ChatLine {
                    from: if e.author_is_me {
                        self.self_username.clone()
                    } else {
                        peer_name.clone()
                    },
                    body: e.body,
                    at: system_time_from_millis(e.at),
                    file: None,
                    chain: Some(ChainMark {
                        hash: e.hash,
                        delivery,
                    }),
                }
            })
            .collect();
        let others: Vec<ChatLine> = self
            .history
            .remove(pubkey)
            .unwrap_or_default()
            .into_iter()
            .filter(|l| l.chain.is_none())
            .collect();
        self.history
            .insert(pubkey.to_string(), merge_by_time(chain_lines, others));
    }

    /// Send (or queue) a text message to the selected peer.
    pub fn send_text(&mut self, body: String) -> Vec<NetCmd> {
        let Some(p) = self.selected_peer() else {
            self.status_msg = "no peer selected".to_string();
            return Vec::new();
        };
        let (id, status, name) = (p.id.clone(), p.status, p.username.clone());
        let chain_pk = p.pubkey.clone().filter(|_| p.chain);
        if let (Some(chains), Some(pk)) = (self.chains.clone(), chain_pk) {
            return match chains.compose(&pk, &body) {
                Ok(block) => {
                    self.rebuild_chain_lines(&pk);
                    if status == PeerStatus::Online {
                        vec![NetCmd::SendBlock { id, block }]
                    } else {
                        self.status_msg =
                            format!("queued for {name}: delivered when they're back online");
                        Vec::new()
                    }
                }
                Err(e) => {
                    self.status_msg = format!("could not write message: {e}");
                    Vec::new()
                }
            };
        }
        if status == PeerStatus::Online {
            self.push_self_message(&id, body.clone());
            vec![NetCmd::SendText { id, body }]
        } else {
            self.status_msg = format!("peer not connected ({status:?})");
            Vec::new()
        }
    }

    /// Desktop alert for `body` from `from`, unless that chat is on screen.
    fn alert_unless_viewing(&self, key: &str, from: &str, body: &str) {
        let viewing =
            self.terminal_focused && self.selected_peer().is_some_and(|p| p.history_key() == key);
        if !viewing {
            crate::notify::message(from, body);
        }
    }

    /// Use `settings` and save every change made in the settings panel.
    pub fn with_settings(mut self, settings: Settings) -> Self {
        self.settings = settings;
        self.persist_settings = true;
        self
    }

    pub fn with_pubkey(mut self, pk: String) -> Self {
        self.self_pubkey = Some(pk);
        self
    }

    pub fn selected_peer(&self) -> Option<&Peer> {
        self.peers.get(self.selected)
    }

    pub fn select_next(&mut self) {
        if self.peers.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected + 1) % self.peers.len();
    }

    pub fn select_prev(&mut self) {
        if self.peers.is_empty() {
            self.selected = 0;
            return;
        }
        if self.selected == 0 {
            self.selected = self.peers.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    fn peer_index(&self, id: &str) -> Option<usize> {
        self.peers.iter().position(|p| p.id == id)
    }

    /// Warn when `pubkey` uses a name that belongs to a different key we know.
    fn impostor_warning(&self, pubkey: &str, username: &str) -> Option<String> {
        let mut known: Vec<(String, String)> = self
            .chains
            .as_ref()
            .map(|c| c.names().into_iter().collect())
            .unwrap_or_default();
        known.extend(
            self.peers
                .iter()
                .filter_map(|p| p.pubkey.clone().map(|k| (k, p.username.clone()))),
        );
        known
            .into_iter()
            .find(|(k, name)| k != pubkey && name.eq_ignore_ascii_case(username))
            .map(|(k, _)| {
                format!(
                    "not the {username} you know: their key is {}, this one is {}",
                    &k[..8.min(k.len())],
                    &pubkey[..8.min(pubkey.len())]
                )
            })
    }

    /// Row for an mDNS instance id: its own row, or one that absorbed it.
    fn mdns_row(&self, id: &str) -> Option<usize> {
        self.peer_index(id).or_else(|| {
            self.peers
                .iter()
                .position(|p| p.mdns_id.as_deref() == Some(id))
        })
    }

    fn history_key_for_id(&self, id: &str) -> String {
        self.peer_index(id)
            .map(|i| self.peers[i].history_key().to_string())
            .unwrap_or_else(|| id.to_string())
    }

    /// Returns a NetCmd if the app event implies one. Mostly events are absorbed into state.
    pub fn on_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::PeerDiscovered {
                id,
                username,
                addr,
                pubkey,
            } => {
                // Same announcement, a row that already absorbed it, or a row
                // for the same key (contact, earlier connection): reuse it.
                let existing = self.mdns_row(&id).or_else(|| {
                    pubkey.as_deref().and_then(|pk| {
                        self.peers.iter().position(|p| {
                            p.pubkey.as_deref() == Some(pk)
                                || p.claimed_pubkey.as_deref() == Some(pk)
                        })
                    })
                });
                if let Some(idx) = existing {
                    let p = &mut self.peers[idx];
                    // A verified row keeps its name; TXT records are unsigned.
                    if p.pubkey.is_none() {
                        p.username = username;
                        p.claimed_pubkey = pubkey;
                    }
                    p.addr = Some(addr);
                    if p.id != id {
                        p.mdns_id = Some(id);
                    }
                    if matches!(p.status, PeerStatus::Offline | PeerStatus::Error) {
                        p.status = PeerStatus::Available;
                    }
                } else {
                    self.peers.push(Peer {
                        claimed_pubkey: pubkey,
                        ..Peer::new(id, username, Some(addr), PeerStatus::Available)
                    });
                }
            }
            AppEvent::PeerLost { id } => {
                if let Some(idx) = self.mdns_row(&id) {
                    // Only mark offline if not currently connected.
                    let p = &mut self.peers[idx];
                    if matches!(p.status, PeerStatus::Available) {
                        p.status = PeerStatus::Offline;
                    }
                }
            }
            AppEvent::PeerConnecting { id, addr, dialed } => {
                if let Some(idx) = self.peer_index(&id) {
                    self.peers[idx].status = PeerStatus::Connecting;
                    self.peers[idx].addr = Some(addr);
                } else {
                    let username = if dialed {
                        format!("dialed:{addr}")
                    } else {
                        format!("incoming:{addr}")
                    };
                    self.peers
                        .push(Peer::new(id, username, Some(addr), PeerStatus::Connecting));
                }
            }
            AppEvent::PeerConnected {
                id,
                username,
                resolved_id: _,
            } => {
                if let Some(idx) = self.peer_index(&id) {
                    self.peers[idx].status = PeerStatus::Online;
                    if !username.is_empty() {
                        self.peers[idx].username = username;
                    }
                }
            }
            AppEvent::PeerAuthenticated {
                id,
                pubkey,
                username,
            } => {
                if let Some(idx) = self.peer_index(&id) {
                    // Key pinning: a row that already stands for a key (an
                    // earlier session, a contact, or the key announced over
                    // mDNS) must not be taken over by a different key.
                    let row = &self.peers[idx];
                    if let Some(expected) = row.pubkey.as_ref().or(row.claimed_pubkey.as_ref()) {
                        if *expected != pubkey {
                            let msg = format!(
                                "key mismatch: expected {}, got {} — not the peer you know",
                                &expected[..8.min(expected.len())],
                                &pubkey[..8.min(pubkey.len())]
                            );
                            tracing::warn!(%id, %expected, got = %pubkey, "peer key mismatch");
                            let row = &mut self.peers[idx];
                            row.status = PeerStatus::Error;
                            row.last_error = Some(msg.clone());
                            self.status_msg = msg;
                            self.outbox.push(NetCmd::Disconnect { id });
                            return;
                        }
                    }
                    let conn_id = self.peers[idx].id.clone();
                    let auth_ip = self.peers[idx].addr.map(|a| a.ip());
                    self.peers[idx].pubkey = Some(pubkey.clone());
                    self.peers[idx].verified =
                        self.chains.as_ref().is_some_and(|c| c.is_verified(&pubkey));
                    self.peers[idx].warning = self.impostor_warning(&pubkey, &username);
                    if !username.is_empty() {
                        self.peers[idx].username = username;
                    }
                    self.peers[idx].status = PeerStatus::Online;

                    // Load persisted history once, on first authentication.
                    if !self.history.contains_key(&pubkey) {
                        self.load_history(&pubkey);
                    }
                    self.rebuild_chain_lines(&pubkey);

                    // Merge any chat that was accumulated under the connection
                    // id before authentication completed.
                    if conn_id != pubkey {
                        if let Some(pre) = self.history.remove(&conn_id) {
                            self.history.entry(pubkey.clone()).or_default().extend(pre);
                        }
                    }

                    // Dedup rows that refer to the same physical peer: any other
                    // row with the same verified pubkey, or a pubkey-less row at
                    // the same address IP (an mDNS discovery shadow of this
                    // freshly authenticated connection).
                    let auth_id = self.peers[idx].id.clone();
                    let selected_id = self.selected_peer().map(|p| p.id.clone());
                    let dup_ids: Vec<String> = self
                        .peers
                        .iter()
                        .filter(|p| p.id != auth_id)
                        .filter(|p| {
                            p.pubkey.as_deref() == Some(pubkey.as_str())
                                || p.claimed_pubkey.as_deref() == Some(pubkey.as_str())
                                // Old peers announce no key: fall back to IP.
                                || (p.pubkey.is_none()
                                    && p.claimed_pubkey.is_none()
                                    && auth_ip.is_some()
                                    && p.addr.map(|a| a.ip()) == auth_ip)
                        })
                        .map(|p| p.id.clone())
                        .collect();
                    for did in &dup_ids {
                        if let Some(pre) = self.history.remove(did) {
                            self.history.entry(pubkey.clone()).or_default().extend(pre);
                        }
                        let Some(pos) = self.peer_index(did) else {
                            continue;
                        };
                        let gone = self.peers.remove(pos);
                        // Keep the mDNS link and the peer's listening address,
                        // so the row can be redialed and matched again after a
                        // disconnect (an inbound row only knows an ephemeral
                        // source port).
                        let mdns = gone
                            .mdns_id
                            .clone()
                            .or_else(|| gone.id.starts_with("mdns:").then(|| gone.id.clone()));
                        if let Some(a) = self.peer_index(&auth_id) {
                            let auth = &mut self.peers[a];
                            if mdns.is_some() && auth.mdns_id.is_none() {
                                auth.mdns_id = mdns;
                                if auth.id.starts_with("inbound:") && gone.addr.is_some() {
                                    auth.addr = gone.addr;
                                }
                            }
                        }
                    }
                    // Keep the user's selection on the same peer.
                    let keep = match selected_id {
                        Some(s) if dup_ids.contains(&s) => Some(auth_id.clone()),
                        other => other,
                    };
                    if let Some(i) = keep.and_then(|k| self.peer_index(&k)) {
                        self.selected = i;
                    }
                    if self.selected >= self.peers.len() {
                        self.selected = self.peers.len().saturating_sub(1);
                    }
                }
            }
            AppEvent::PeerDisconnected { id } => {
                if let Some(idx) = self.peer_index(&id) {
                    let p = &mut self.peers[idx];
                    p.status = if p.addr.is_some() {
                        PeerStatus::Available
                    } else {
                        PeerStatus::Offline
                    };
                }
            }
            AppEvent::PeerError { id, error } => {
                if let Some(idx) = self.peer_index(&id) {
                    let p = &mut self.peers[idx];
                    p.status = PeerStatus::Error;
                    p.last_error = Some(error);
                }
            }
            AppEvent::MessageReceived { id, from, body } => {
                let key = self.history_key_for_id(&id);
                // A message ends any pending typing indicator for that peer.
                if let Some(idx) = self.peer_index(&id) {
                    self.peers[idx].typing_at = None;
                }
                // Notify if the user isn't already looking at this chat, either
                // because the terminal is unfocused or a different peer is open.
                self.alert_unless_viewing(&key, &from, &body);
                self.history
                    .entry(key)
                    .or_default()
                    .push(ChatLine::text(from, body));
            }
            AppEvent::PeerChainReady { id } => {
                if let Some(idx) = self.peer_index(&id) {
                    self.peers[idx].chain = true;
                }
            }
            AppEvent::ChainChanged {
                id,
                pubkey,
                from_peer,
            } => {
                if let Some(idx) = self.peer_index(&id) {
                    self.peers[idx].typing_at = None;
                }
                self.rebuild_chain_lines(&pubkey);
                if !from_peer.is_empty() {
                    let from = self
                        .peer_index(&id)
                        .map(|i| self.peers[i].username.clone())
                        .unwrap_or_default();
                    let body = if from_peer.len() == 1 {
                        from_peer[0].clone()
                    } else {
                        format!("{} new messages", from_peer.len())
                    };
                    self.alert_unless_viewing(&pubkey, &from, &body);
                }
            }
            AppEvent::BlockDelivered { hash, .. } => {
                if let Some(m) = self
                    .history
                    .values_mut()
                    .flatten()
                    .filter_map(|l| l.chain.as_mut())
                    .find(|m| m.hash == hash)
                {
                    if m.delivery == Delivery::Queued {
                        m.delivery = Delivery::Sent;
                    }
                }
            }
            AppEvent::FileOffered {
                id,
                file_id,
                name,
                size,
            } => self.on_file_offered(id, file_id, name, size),
            AppEvent::FileAccepted { file_id, .. } => {
                self.set_file_state(&file_id, FileState::Transferring(0));
            }
            AppEvent::FileRejected { file_id, .. } => {
                self.set_file_state(&file_id, FileState::Rejected);
            }
            AppEvent::FileProgress { file_id, bytes, .. } => {
                self.set_file_state(&file_id, FileState::Transferring(bytes));
            }
            AppEvent::FileSent { file_id, .. } => {
                self.set_file_state(&file_id, FileState::Done);
            }
            AppEvent::FileReceived { file_id, path, .. } => {
                let msg = format!("saved {}", path.display());
                if let Some(f) = self.file_mut(&file_id) {
                    f.state = FileState::Done;
                    f.path = Some(path);
                    self.status_msg = msg;
                }
            }
            AppEvent::FileFailed { file_id, error, .. } => {
                // Don't overwrite a state the user already chose.
                if let Some(f) = self.file_mut(&file_id) {
                    if !matches!(f.state, FileState::Rejected | FileState::Failed(_)) {
                        f.state = FileState::Failed(error);
                    }
                }
            }
            AppEvent::ViewerLoaded { path, page, result } => {
                // Ignore a stale page if the user already flipped past it.
                if let Some(v) = self
                    .viewer
                    .as_mut()
                    .filter(|v| v.path == path && v.page == page)
                {
                    match result {
                        Ok(c) => v.content = Some(c),
                        Err(e) => v.error = Some(e),
                    }
                }
            }
            AppEvent::PeerTyping { id, active } => {
                if let Some(idx) = self.peer_index(&id) {
                    self.peers[idx].typing_at = if active { Some(Instant::now()) } else { None };
                }
            }
        }
    }

    /// Append a sent message to the in-memory history. Disk persistence is
    /// the net task's responsibility (peer.rs writes after the wire send).
    pub fn push_self_message(&mut self, peer_id: &str, body: String) {
        let key = self.history_key_for_id(peer_id);
        self.history
            .entry(key)
            .or_default()
            .push(ChatLine::text(self.self_username.clone(), body));
    }

    /// Called while the user edits the input box. Emits a throttled typing
    /// ping (at most once per second) to the selected peer if it is online.
    pub fn typing_ping(&mut self) -> Vec<NetCmd> {
        let now = Instant::now();
        if let Some(t) = self.last_typing_sent {
            if now.duration_since(t) < Duration::from_secs(1) {
                return Vec::new();
            }
        }
        if let Some(p) = self.selected_peer() {
            if matches!(p.status, PeerStatus::Online) {
                let id = p.id.clone();
                self.last_typing_sent = Some(now);
                return vec![NetCmd::SendTyping { id, active: true }];
            }
        }
        Vec::new()
    }

    /// Tell the selected peer we have stopped typing (input sent or cleared).
    pub fn typing_stop(&mut self) -> Vec<NetCmd> {
        if self.last_typing_sent.take().is_none() {
            return Vec::new();
        }
        if let Some(p) = self.selected_peer() {
            if matches!(p.status, PeerStatus::Online) {
                let id = p.id.clone();
                return vec![NetCmd::SendTyping { id, active: false }];
            }
        }
        Vec::new()
    }

    /// Handle a left click at screen cell (`x`, `y`): queue the link under it.
    pub fn click(&mut self, x: u16, y: u16) {
        let hit = self
            .link_regions
            .iter()
            .find(|r| r.y == y && (r.x_start..r.x_end).contains(&x));
        match hit.map(|r| r.target.clone()) {
            Some(LinkTarget::Url(url)) => self.request_open(url),
            Some(LinkTarget::File(path)) => self.open_viewer(path),
            None => {}
        }
    }

    /// Show `path` in the viewer; the main loop loads it.
    pub fn open_viewer(&mut self, path: PathBuf) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.viewer = Some(Viewer {
            name,
            path: path.clone(),
            content: None,
            error: None,
            scroll: 0,
            page: 1,
        });
        self.pending_view = Some(path);
    }

    pub fn close_viewer(&mut self) {
        if self.viewer.take().is_some() {
            self.needs_clear = true;
        }
    }

    /// Flip a paged document by `delta` pages; returns false if the viewer
    /// isn't showing one (so the key can scroll instead).
    pub fn viewer_page(&mut self, delta: i32) -> bool {
        let Some(v) = self.viewer.as_mut() else {
            return false;
        };
        let Some(Content::Image {
            page: Some((cur, total)),
            ..
        }) = v.content
        else {
            return false;
        };
        let next = (cur as i32 + delta).clamp(1, total as i32) as u32;
        if next != cur {
            v.page = next;
            v.content = None; // "loading…" until the new page arrives
            v.error = None;
            self.pending_view = Some(v.path.clone());
            self.needs_clear = true; // drop the old page's graphics
        }
        true
    }

    /// Scroll the text viewer by `delta` lines.
    pub fn scroll_viewer(&mut self, delta: i32) {
        if let Some(v) = self.viewer.as_mut() {
            v.scroll = (v.scroll as i32 + delta).clamp(0, u16::MAX as i32) as u16;
        }
    }

    /// Finished transfers in the selected chat (viewable files), oldest
    /// first, as (display name, path).
    pub fn selected_chat_files(&self) -> Vec<(String, PathBuf)> {
        self.selected_chat()
            .iter()
            .filter_map(|cl| cl.file.as_ref())
            .filter(|f| f.state == FileState::Done)
            .filter_map(|f| f.path.clone().map(|p| (f.name.clone(), p)))
            .collect()
    }

    fn selected_chat(&self) -> &[ChatLine] {
        self.selected_peer()
            .and_then(|p| self.history.get(p.history_key()))
            .map(|h| h.as_slice())
            .unwrap_or(&[])
    }

    fn file_mut(&mut self, file_id: &str) -> Option<&mut FileInfo> {
        if file_id.is_empty() {
            return None;
        }
        self.history
            .values_mut()
            .flat_map(|h| h.iter_mut())
            .filter_map(|cl| cl.file.as_mut())
            .find(|f| f.file_id == file_id)
    }

    fn set_file_state(&mut self, file_id: &str, state: FileState) {
        if let Some(f) = self.file_mut(file_id) {
            f.state = state;
        }
    }

    /// Pending incoming offers in the selected chat, oldest first.
    /// File lines in the selected chat, newest first (the `:files` panel).
    pub fn chat_files(&self) -> Vec<FileInfo> {
        self.selected_chat()
            .iter()
            .rev()
            .filter_map(|cl| cl.file.clone())
            .collect()
    }

    /// Accept or reject the given pending offers.
    fn answer_offers(&mut self, file_ids: Vec<String>, accept: bool) -> Vec<NetCmd> {
        let Some(id) = self.selected_peer().map(|p| p.id.clone()) else {
            return Vec::new();
        };
        let n = file_ids.len();
        let dir = self.settings.download_dir.clone();
        let cmds = file_ids
            .into_iter()
            .map(|file_id| {
                if accept {
                    self.set_file_state(&file_id, FileState::Transferring(0));
                    NetCmd::AcceptFile {
                        id: id.clone(),
                        file_id,
                        dir: dir.clone(),
                    }
                } else {
                    self.set_file_state(&file_id, FileState::Rejected);
                    NetCmd::RejectFile {
                        id: id.clone(),
                        file_id,
                    }
                }
            })
            .collect();
        self.status_msg = match (accept, n) {
            (true, 1) => format!("receiving into {}", dir.display()),
            (true, n) => format!("receiving {n} files into {}", dir.display()),
            (false, 1) => "file declined".into(),
            (false, n) => format!("{n} files declined"),
        };
        cmds
    }

    /// Open the `:files` panel with a hint in the status line.
    fn open_files_panel(&mut self, hint: String) {
        self.focus = Focus::Files;
        self.files_sel = 0;
        self.status_msg = hint;
    }

    /// `:accept` / `:reject` with an optional number, name or `all`.
    fn decide_offer(&mut self, arg: &str, accept: bool) -> Vec<NetCmd> {
        let offers: Vec<FileInfo> = self
            .chat_files()
            .into_iter()
            .filter(|f| f.state == FileState::AwaitingDecision)
            .collect();
        let verb = if accept { "accept" } else { "reject" };
        match pick(&offers, arg) {
            Pick::One(i) => self.answer_offers(vec![offers[i].file_id.clone()], accept),
            Pick::All => {
                let ids = offers.into_iter().map(|f| f.file_id).collect();
                self.answer_offers(ids, accept)
            }
            Pick::Ambiguous => {
                self.open_files_panel(format!(
                    "{} pending offers: pick one (a accept · r reject · A/R all)",
                    offers.len()
                ));
                Vec::new()
            }
            Pick::Nothing if offers.is_empty() => {
                self.status_msg = "no pending file offers in this chat".into();
                Vec::new()
            }
            Pick::Nothing => {
                self.status_msg =
                    format!("no offer matches {arg:?} — try :{verb} with no argument");
                Vec::new()
            }
        }
    }

    /// `:view` with an optional number or name.
    fn view_file(&mut self, arg: &str) {
        let done: Vec<FileInfo> = self
            .chat_files()
            .into_iter()
            .filter(|f| f.state == FileState::Done && f.path.is_some())
            .collect();
        match pick(&done, arg) {
            Pick::One(i) => {
                let path = done[i].path.clone().expect("filtered");
                self.open_viewer(path);
            }
            Pick::Ambiguous | Pick::All => {
                self.open_files_panel(format!("{} files: pick one to open", done.len()))
            }
            Pick::Nothing if done.is_empty() => self.status_msg = "no files in this chat".into(),
            Pick::Nothing => self.status_msg = format!("no file matches {arg:?}"),
        }
    }

    /// A key pressed in the `:files` panel.
    pub fn files_action(&mut self, action: FilesAction) -> Vec<NetCmd> {
        let list = self.chat_files();
        if list.is_empty() {
            self.focus = Focus::Input;
            return Vec::new();
        }
        self.files_sel = self.files_sel.min(list.len() - 1);
        let cur = list[self.files_sel].clone();
        let pending: Vec<String> = list
            .iter()
            .filter(|f| f.state == FileState::AwaitingDecision)
            .map(|f| f.file_id.clone())
            .collect();
        match action {
            FilesAction::Up => self.files_sel = self.files_sel.saturating_sub(1),
            FilesAction::Down => self.files_sel = (self.files_sel + 1).min(list.len() - 1),
            FilesAction::Close => self.focus = Focus::Input,
            FilesAction::Open => match (&cur.state, &cur.path) {
                (FileState::AwaitingDecision, _) => {
                    return self.answer_offers(vec![cur.file_id], true)
                }
                (FileState::Done, Some(p)) => {
                    let p = p.clone();
                    self.focus = Focus::Input;
                    self.open_viewer(p);
                }
                _ => self.status_msg = format!("{} can't be opened yet", cur.name),
            },
            FilesAction::Accept | FilesAction::Reject
                if cur.state != FileState::AwaitingDecision =>
            {
                self.status_msg = format!("{} isn't waiting for an answer", cur.name);
            }
            FilesAction::Accept => return self.answer_offers(vec![cur.file_id], true),
            FilesAction::Reject => return self.answer_offers(vec![cur.file_id], false),
            FilesAction::AcceptAll | FilesAction::RejectAll if pending.is_empty() => {
                self.status_msg = "no pending offers".into();
            }
            FilesAction::AcceptAll => return self.answer_offers(pending, true),
            FilesAction::RejectAll => return self.answer_offers(pending, false),
        }
        Vec::new()
    }

    /// `:send <path>`: offer a local file to the selected peer.
    fn send_file(&mut self, arg: &str) -> Vec<NetCmd> {
        if arg.is_empty() {
            self.status_msg = "usage: :send <path>".into();
            return Vec::new();
        }
        let Some(p) = self.selected_peer() else {
            self.status_msg = "no peer selected".into();
            return Vec::new();
        };
        if p.status != PeerStatus::Online {
            self.status_msg = format!("peer not connected ({:?})", p.status);
            return Vec::new();
        }
        let id = p.id.clone();
        let path = files::expand_tilde(arg);
        let meta = match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => m,
            Ok(_) => {
                self.status_msg = format!("not a file: {}", path.display());
                return Vec::new();
            }
            Err(e) => {
                self.status_msg = format!("{}: {e}", path.display());
                return Vec::new();
            }
        };
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let name = files::sanitize_name(&path.file_name().unwrap_or_default().to_string_lossy());
        let size = meta.len();
        let file_id = files::new_file_id();
        let key = self.history_key_for_id(&id);
        self.history.entry(key).or_default().push(ChatLine {
            file: Some(FileInfo {
                file_id: file_id.clone(),
                name: name.clone(),
                size,
                path: Some(path.clone()),
                state: FileState::Offered,
            }),
            ..ChatLine::text(self.self_username.clone(), name.clone())
        });
        self.chat_scroll = 0;
        self.status_msg = format!("offered {name} ({})", files::human_size(size));
        vec![NetCmd::SendFile {
            id,
            file_id,
            path,
            name,
            size,
        }]
    }

    /// Persist settings after a change made in the panel.
    pub fn settings_changed(&mut self) {
        if !self.persist_settings {
            return;
        }
        if let Err(e) = self.settings.save() {
            tracing::warn!(error = %e, "saving settings failed");
            self.status_msg = format!("could not save settings: {e}");
        }
    }

    /// Change the highlighted settings row (`forward` = Right/Enter).
    pub fn settings_adjust(&mut self, forward: bool) {
        match self.settings_sel {
            0 => {
                let p = self.settings.incoming_files;
                self.settings.incoming_files = if forward { p.next() } else { p.prev() };
            }
            1 => self.settings.cycle_max_file_mb(forward),
            _ => {
                self.settings_edit = Some(self.settings.download_dir.display().to_string());
                return;
            }
        }
        self.settings_changed();
    }

    /// Finish editing the download folder.
    pub fn settings_commit_edit(&mut self) {
        if let Some(s) = self.settings_edit.take() {
            let s = s.trim();
            if !s.is_empty() {
                self.settings.download_dir = files::expand_tilde(s);
                self.settings_changed();
            }
        }
    }

    fn on_file_offered(&mut self, id: String, file_id: String, name: String, size: u64) {
        use crate::settings::FilePolicy;
        let key = self.history_key_for_id(&id);
        let from = self
            .peer_index(&id)
            .map(|i| self.peers[i].username.clone())
            .unwrap_or_else(|| id.clone());
        let too_big = size > self.settings.max_file_bytes();
        let policy = self.settings.incoming_files;
        let state = if too_big || policy == FilePolicy::Reject {
            self.outbox.push(NetCmd::RejectFile {
                id: id.clone(),
                file_id: file_id.clone(),
            });
            if too_big {
                FileState::Failed(format!(
                    "declined: over the {} MB limit",
                    self.settings.max_file_mb
                ))
            } else {
                FileState::Rejected
            }
        } else if policy == FilePolicy::AutoAccept {
            self.outbox.push(NetCmd::AcceptFile {
                id: id.clone(),
                file_id: file_id.clone(),
                dir: self.settings.download_dir.clone(),
            });
            FileState::Transferring(0)
        } else {
            let viewing = self.terminal_focused
                && self
                    .selected_peer()
                    .is_some_and(|p| p.history_key() == key.as_str());
            if !viewing {
                crate::notify::message(
                    &from,
                    &format!("wants to send {name} ({})", files::human_size(size)),
                );
            }
            self.status_msg = format!(
                "{from} wants to send {name} ({}) — :accept or :reject",
                files::human_size(size)
            );
            FileState::AwaitingDecision
        };
        self.history.entry(key).or_default().push(ChatLine {
            file: Some(FileInfo {
                file_id,
                name: name.clone(),
                size,
                path: None,
                state,
            }),
            ..ChatLine::text(from, name)
        });
    }

    fn request_open(&mut self, url: String) {
        self.status_msg = format!("opening {url}");
        self.pending_open = Some(url);
    }

    /// Links in the selected chat, oldest first.
    pub fn selected_chat_urls(&self) -> Vec<String> {
        self.selected_peer()
            .and_then(|p| self.history.get(p.history_key()))
            .map(|h| {
                h.iter()
                    .filter(|cl| cl.file.is_none())
                    .flat_map(|cl| crate::links::urls_in(&cl.body))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Suggestions for the command being typed (empty once arguments start).
    pub fn command_suggestions(&self) -> Vec<crate::commands::Suggestion> {
        if self.command.contains(' ') {
            Vec::new()
        } else {
            crate::commands::suggest(&self.command)
        }
    }

    /// The command whose arguments are being typed, for a signature hint.
    pub fn command_signature(&self) -> Option<&'static crate::commands::CommandSpec> {
        let (word, _) = self.command.split_once(' ')?;
        crate::commands::lookup(word.trim())
    }

    /// Move the suggestion highlight by `delta`, wrapping.
    pub fn command_move(&mut self, delta: i32) {
        let n = self.command_suggestions().len() as i32;
        if n > 0 {
            self.cmd_sel = (self.cmd_sel as i32 + delta).rem_euclid(n) as usize;
        }
    }

    /// Replace the typed word with the highlighted command (Tab).
    pub fn command_complete(&mut self) -> bool {
        let Some(s) = self.command_suggestions().into_iter().nth(self.cmd_sel) else {
            return false;
        };
        self.command = if s.spec.args.is_empty() {
            s.spec.name.to_string()
        } else {
            format!("{} ", s.spec.name)
        };
        self.command_cursor = self.command.len();
        self.cmd_sel = 0;
        true
    }

    /// Enter on the command line: run what's typed if it names a command;
    /// otherwise take the highlighted suggestion, running it unless it
    /// still needs an argument.
    pub fn command_enter(&mut self) -> Vec<NetCmd> {
        let word = self.command.split_whitespace().next().unwrap_or("");
        let exact = crate::commands::lookup(word).is_some();
        if !exact && !word.is_empty() && self.command_complete() {
            let needs = crate::commands::lookup(self.command.trim()).is_some_and(|c| c.needs_arg());
            if needs {
                return Vec::new(); // stay on the line to type the argument
            }
        }
        let line = std::mem::take(&mut self.command);
        self.command_cursor = 0;
        self.cmd_sel = 0;
        self.focus = Focus::Input;
        self.run_command(&line)
    }

    /// Returns NetCmds emitted by handling a command-line entry like `:c 1.2.3.4:7421`.
    pub fn run_command(&mut self, line: &str) -> Vec<NetCmd> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let mut parts = line.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        match cmd {
            "q" | "quit" => {
                self.should_quit = true;
                Vec::new()
            }
            "c" | "connect" => match arg.parse::<SocketAddr>() {
                Ok(addr) => {
                    let id = format!("manual:{addr}");
                    self.status_msg = format!("dialing {addr}");
                    vec![NetCmd::Connect { id, addr }]
                }
                Err(e) => {
                    self.status_msg = format!("bad addr: {e}");
                    Vec::new()
                }
            },
            "nick" => {
                if !arg.is_empty() {
                    self.self_username = arg.to_string();
                    self.status_msg = format!("nick set to {arg}");
                }
                Vec::new()
            }
            "disconnect" | "d" => {
                if let Some(p) = self.selected_peer() {
                    let id = p.id.clone();
                    vec![NetCmd::Disconnect { id }]
                } else {
                    Vec::new()
                }
            }
            "o" | "open" => {
                // `:open` = newest link in this chat, `:open N` = Nth newest.
                let n = if arg.is_empty() {
                    Ok(1)
                } else {
                    arg.parse::<usize>()
                };
                let urls = self.selected_chat_urls();
                match n {
                    Ok(n) if n >= 1 && n <= urls.len() => {
                        let url = urls[urls.len() - n].clone();
                        self.request_open(url);
                    }
                    Ok(_) if urls.is_empty() => self.status_msg = "no links in this chat".into(),
                    Ok(_) => self.status_msg = format!("only {} link(s) in this chat", urls.len()),
                    Err(_) => self.status_msg = format!("bad link number: {arg}"),
                }
                Vec::new()
            }
            "send" | "f" => self.send_file(arg),
            "accept" | "a" => self.decide_offer(arg, true),
            "reject" => self.decide_offer(arg, false),
            "view" | "v" => {
                self.view_file(arg);
                Vec::new()
            }
            "files" => {
                if self.chat_files().is_empty() {
                    self.status_msg = "no files in this chat".into();
                } else {
                    self.open_files_panel(String::new());
                }
                Vec::new()
            }
            "verify" if !arg.is_empty() => {
                let confirm = match arg {
                    "confirm" | "ok" | "yes" => true,
                    "forget" | "reset" => false,
                    _ => {
                        self.status_msg = "usage: :verify [confirm|forget]".into();
                        return Vec::new();
                    }
                };
                let Some(pk) = self.selected_peer().and_then(|p| p.pubkey.clone()) else {
                    self.status_msg = "verify needs a peer that completed the key handshake".into();
                    return Vec::new();
                };
                if let Some(c) = &self.chains {
                    if let Err(e) = c.set_verified(&pk, confirm) {
                        self.status_msg = format!("could not save: {e}");
                        return Vec::new();
                    }
                }
                for p in self
                    .peers
                    .iter_mut()
                    .filter(|p| p.pubkey.as_deref() == Some(pk.as_str()))
                {
                    p.verified = confirm;
                }
                self.status_msg = if confirm {
                    "marked as verified ✔".into()
                } else {
                    "verification removed".into()
                };
                Vec::new()
            }
            "verify" => {
                let keys = (
                    self.self_pubkey.as_deref().and_then(decode32),
                    self.selected_peer()
                        .and_then(|p| p.pubkey.as_deref())
                        .and_then(decode32),
                );
                match keys {
                    (Some(me), Some(them)) => {
                        let p = self.selected_peer().expect("checked above");
                        let key = p.history_key().to_string();
                        let name = p.username.clone();
                        let code = chain::safety_code(&me, &them);
                        self.history.entry(key).or_default().push(ChatLine::text(
                            "🔒".into(),
                            format!(
                                "safety code with {name}: {code} — if they see the same code, \
                                 you're talking to the right person (then :verify confirm)"
                            ),
                        ));
                        self.chat_scroll = 0;
                    }
                    _ => {
                        self.status_msg =
                            "verify needs a peer that completed the key handshake".into()
                    }
                }
                Vec::new()
            }
            "help" | "h" => {
                // The hint panel lists every command on an empty `:` line.
                self.focus = Focus::Command;
                self.command.clear();
                self.command_cursor = 0;
                self.cmd_sel = 0;
                Vec::new()
            }
            "settings" | "set" => {
                self.focus = Focus::Settings;
                self.settings_sel = 0;
                self.settings_edit = None;
                Vec::new()
            }
            other => {
                self.status_msg = format!("unknown command: {other}");
                Vec::new()
            }
        }
    }
}

fn decode32(hex_str: &str) -> Option<[u8; 32]> {
    hex::decode(hex_str).ok()?.try_into().ok()
}

fn system_time_from_millis(ms: i64) -> SystemTime {
    if ms >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}

/// Merge two time-ordered line lists, keeping each list's own order (chain
/// order is causal, so it must not be re-sorted).
fn merge_by_time(chain: Vec<ChatLine>, others: Vec<ChatLine>) -> Vec<ChatLine> {
    let mut out = Vec::with_capacity(chain.len() + others.len());
    let mut a = chain.into_iter().peekable();
    let mut b = others.into_iter().peekable();
    loop {
        let take_b = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => y.at < x.at,
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => break,
        };
        out.extend(if take_b { b.next() } else { a.next() });
    }
    out
}

fn system_time_from_chrono(dt: DateTime<Utc>) -> SystemTime {
    let secs = dt.timestamp();
    let nanos = dt.timestamp_subsec_nanos();
    if secs >= 0 {
        SystemTime::UNIX_EPOCH + std::time::Duration::new(secs as u64, nanos)
    } else {
        SystemTime::UNIX_EPOCH - std::time::Duration::new((-secs) as u64, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn unique_pubkey() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        format!("{nanos:032x}{n:032x}")
    }

    #[test]
    fn authenticated_peer_dedups_mdns_shadow() {
        let mut app = App::new("me".to_string(), 7421);
        let listen_addr: SocketAddr = "192.0.2.7:7421".parse().unwrap();
        let inbound_src: SocketAddr = "192.0.2.7:44514".parse().unwrap();
        let mdns_id = "mdns:alice@host._cweldrop._tcp.local.".to_string();
        let inbound_id = "inbound:192.0.2.7:44514".to_string();
        let pubkey = unique_pubkey();

        app.on_event(AppEvent::PeerDiscovered {
            id: mdns_id.clone(),
            username: "alice".into(),
            addr: listen_addr,
            pubkey: None,
        });
        assert_eq!(app.peers.len(), 1, "discovery creates one row");

        app.on_event(AppEvent::PeerConnecting {
            id: inbound_id.clone(),
            addr: inbound_src,
            dialed: false,
        });
        assert_eq!(
            app.peers.len(),
            2,
            "inbound peer adds a second row before auth"
        );

        app.on_event(AppEvent::PeerAuthenticated {
            id: inbound_id.clone(),
            pubkey: pubkey.clone(),
            username: "alice".into(),
        });

        assert_eq!(
            app.peers.len(),
            1,
            "auth must collapse mDNS shadow into the connected row"
        );
        let p = &app.peers[0];
        assert_eq!(p.id, inbound_id, "kept the connected row's id");
        assert_eq!(p.pubkey.as_deref(), Some(pubkey.as_str()));
        assert_eq!(p.status, PeerStatus::Online);
        assert_eq!(p.username, "alice");
    }

    #[test]
    fn authenticated_peer_dedups_other_pubkey_row() {
        let mut app = App::new("me".to_string(), 7421);
        let addr_a: SocketAddr = "192.0.2.7:7421".parse().unwrap();
        let addr_b: SocketAddr = "192.0.2.7:55555".parse().unwrap();
        let pubkey = unique_pubkey();

        app.on_event(AppEvent::PeerConnecting {
            id: "first".into(),
            addr: addr_a,
            dialed: true,
        });
        app.on_event(AppEvent::PeerAuthenticated {
            id: "first".into(),
            pubkey: pubkey.clone(),
            username: "alice".into(),
        });

        app.on_event(AppEvent::PeerConnecting {
            id: "second".into(),
            addr: addr_b,
            dialed: true,
        });
        app.on_event(AppEvent::PeerAuthenticated {
            id: "second".into(),
            pubkey: pubkey.clone(),
            username: "alice".into(),
        });

        assert_eq!(
            app.peers.len(),
            1,
            "two connections to the same pubkey must collapse to one row"
        );
        assert_eq!(app.peers[0].id, "second");
    }

    fn online_peer(app: &mut App, id: &str) {
        let addr: SocketAddr = "192.0.2.9:7421".parse().unwrap();
        app.on_event(AppEvent::PeerConnecting {
            id: id.into(),
            addr,
            dialed: true,
        });
        app.on_event(AppEvent::PeerAuthenticated {
            id: id.into(),
            pubkey: unique_pubkey(),
            username: "them".into(),
        });
    }

    #[test]
    fn peer_typing_sets_and_clears() {
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");
        app.on_event(AppEvent::PeerTyping {
            id: "p".into(),
            active: true,
        });
        assert!(app.peers[0].is_typing());
        app.on_event(AppEvent::PeerTyping {
            id: "p".into(),
            active: false,
        });
        assert!(!app.peers[0].is_typing());
    }

    #[test]
    fn message_clears_typing_indicator() {
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");
        app.on_event(AppEvent::PeerTyping {
            id: "p".into(),
            active: true,
        });
        app.on_event(AppEvent::MessageReceived {
            id: "p".into(),
            from: "them".into(),
            body: "hi".into(),
        });
        assert!(!app.peers[0].is_typing());
    }

    #[test]
    fn typing_ping_only_when_online_and_throttled() {
        let mut app = App::new("me".into(), 7421);
        let addr: SocketAddr = "192.0.2.9:7421".parse().unwrap();
        app.on_event(AppEvent::PeerConnecting {
            id: "p".into(),
            addr,
            dialed: true,
        });
        // Connecting, not yet online: no ping.
        assert!(app.typing_ping().is_empty());

        app.on_event(AppEvent::PeerAuthenticated {
            id: "p".into(),
            pubkey: unique_pubkey(),
            username: "them".into(),
        });
        assert_eq!(app.typing_ping().len(), 1, "first ping fires");
        assert!(app.typing_ping().is_empty(), "throttled within 1s");

        // Stop emits exactly once after a ping was sent.
        assert_eq!(app.typing_stop().len(), 1);
        assert!(app.typing_stop().is_empty(), "nothing to stop twice");
    }

    #[test]
    fn open_command_picks_nth_newest_link() {
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");
        app.push_self_message("p", "old https://a.com and www.b.com".into());
        app.push_self_message("p", "new http://c.org/x.".into());

        app.run_command("open");
        assert_eq!(app.pending_open.take().as_deref(), Some("http://c.org/x"));
        app.run_command("o 2");
        assert_eq!(
            app.pending_open.take().as_deref(),
            Some("https://www.b.com")
        );
        app.run_command("open 9");
        assert_eq!(app.pending_open, None);
        assert!(app.status_msg.contains("only 3"));
        app.run_command("open x");
        assert_eq!(app.pending_open, None);
    }

    #[test]
    fn open_command_without_links() {
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");
        app.run_command("open");
        assert_eq!(app.pending_open, None);
        assert_eq!(app.status_msg, "no links in this chat");
    }

    fn offer(app: &mut App, file_id: &str, size: u64) {
        app.on_event(AppEvent::FileOffered {
            id: "p".into(),
            file_id: file_id.into(),
            name: "cat.png".into(),
            size,
        });
    }

    fn state_of(app: &App, file_id: &str) -> FileState {
        app.history
            .values()
            .flatten()
            .filter_map(|cl| cl.file.as_ref())
            .find(|f| f.file_id == file_id)
            .map(|f| f.state.clone())
            .expect("file line")
    }

    #[test]
    fn offer_policy_ask_then_accept() {
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");
        offer(&mut app, "f1", 10);
        assert!(app.outbox.is_empty(), "ask waits for the user");
        assert_eq!(state_of(&app, "f1"), FileState::AwaitingDecision);
        let cmds = app.run_command("accept");
        assert!(
            matches!(&cmds[..], [NetCmd::AcceptFile { file_id, dir, .. }]
            if file_id == "f1" && *dir == app.settings.download_dir)
        );
        assert_eq!(state_of(&app, "f1"), FileState::Transferring(0));
        assert!(
            app.run_command("accept").is_empty(),
            "nothing left to accept"
        );
    }

    #[test]
    fn offer_policy_reject_command() {
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");
        offer(&mut app, "f1", 10);
        offer(&mut app, "f2", 10);
        // `:reject 2` = second newest = f1.
        let cmds = app.run_command("reject 2");
        assert!(matches!(&cmds[..], [NetCmd::RejectFile { file_id, .. }] if file_id == "f1"));
        assert_eq!(state_of(&app, "f1"), FileState::Rejected);
        assert_eq!(state_of(&app, "f2"), FileState::AwaitingDecision);
    }

    #[test]
    fn offer_policy_auto_accept_and_reject_and_size_cap() {
        use crate::settings::FilePolicy;
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");

        app.settings.incoming_files = FilePolicy::AutoAccept;
        offer(&mut app, "f1", 10);
        assert!(matches!(&app.outbox[..], [NetCmd::AcceptFile { .. }]));
        app.outbox.clear();

        // Over the cap is declined even with auto-accept.
        let too_big = app.settings.max_file_bytes() + 1;
        offer(&mut app, "f2", too_big);
        assert!(matches!(&app.outbox[..], [NetCmd::RejectFile { .. }]));
        assert!(matches!(state_of(&app, "f2"), FileState::Failed(_)));
        app.outbox.clear();

        app.settings.incoming_files = FilePolicy::Reject;
        offer(&mut app, "f3", 10);
        assert!(matches!(&app.outbox[..], [NetCmd::RejectFile { .. }]));
        assert_eq!(state_of(&app, "f3"), FileState::Rejected);
    }

    #[test]
    fn transfer_events_update_line() {
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");
        offer(&mut app, "f1", 10);
        app.run_command("a");
        app.on_event(AppEvent::FileProgress {
            id: "p".into(),
            file_id: "f1".into(),
            bytes: 5,
        });
        assert_eq!(state_of(&app, "f1"), FileState::Transferring(5));
        app.on_event(AppEvent::FileReceived {
            id: "p".into(),
            file_id: "f1".into(),
            path: "/tmp/cat.png".into(),
        });
        assert_eq!(state_of(&app, "f1"), FileState::Done);
        assert_eq!(
            app.selected_chat_files(),
            vec![("cat.png".to_string(), PathBuf::from("/tmp/cat.png"))]
        );
        app.run_command("view");
        assert_eq!(app.pending_view, Some("/tmp/cat.png".into()));
        app.close_viewer();
        assert!(app.viewer.is_none() && app.needs_clear);
    }

    #[test]
    fn send_command_offers_existing_file() {
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");
        let dir = std::env::temp_dir().join(format!("cweldrop-send-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doc.txt");
        std::fs::write(&path, b"12345").unwrap();

        let cmds = app.run_command(&format!("send {}", path.display()));
        let file_id = match &cmds[..] {
            [NetCmd::SendFile {
                file_id,
                name,
                size,
                ..
            }] => {
                assert_eq!((name.as_str(), *size), ("doc.txt", 5));
                file_id.clone()
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(state_of(&app, &file_id), FileState::Offered);

        assert!(app.run_command("send /definitely/not/here").is_empty());
        assert!(app
            .run_command(&format!("send {}", dir.display()))
            .is_empty());
        assert!(app.status_msg.contains("not a file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_adjust_cycles_and_edits() {
        use crate::settings::FilePolicy;
        let mut app = App::new("me".into(), 7421);
        app.settings_adjust(true);
        assert_eq!(app.settings.incoming_files, FilePolicy::AutoAccept);
        app.settings_sel = 1;
        app.settings_adjust(true);
        assert_eq!(app.settings.max_file_mb, 500);
        app.settings_sel = 2;
        app.settings_adjust(true);
        app.settings_edit = Some("/tmp/dl".into());
        app.settings_commit_edit();
        assert_eq!(app.settings.download_dir, PathBuf::from("/tmp/dl"));
    }

    fn test_identity() -> Arc<crate::identity::Identity> {
        let dir = std::env::temp_dir().join(format!("cweldrop-app-id-{}", rand::random::<u64>()));
        let id = crate::identity::Identity::load_or_create_in(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(id)
    }

    fn test_chains(id: &Arc<crate::identity::Identity>) -> Arc<Chains> {
        let dir =
            std::env::temp_dir().join(format!("cweldrop-app-chains-{}", rand::random::<u64>()));
        Chains::open_in(Arc::clone(id), dir)
    }

    fn deliveries(app: &App, key: &str) -> Vec<(String, Delivery)> {
        app.history[key]
            .iter()
            .filter_map(|l| l.chain.as_ref().map(|m| (l.body.clone(), m.delivery)))
            .collect()
    }

    #[test]
    fn queue_for_offline_contact_then_deliver_and_ack() {
        let (me, them) = (test_identity(), test_identity());
        let (my_chains, their_chains) = (test_chains(&me), test_chains(&them));
        let them_pk = them.pubkey_hex();
        my_chains.compose(&them_pk, "earlier").unwrap();

        let mut app = App::new("me".into(), 7421)
            .with_pubkey(me.pubkey_hex())
            .with_chains(Arc::clone(&my_chains));
        assert_eq!(app.peers.len(), 1, "stored chain becomes a contact");
        let contact = app.peers[0].clone();
        assert_eq!(contact.status, PeerStatus::Offline);
        assert!(contact.chain);
        assert_eq!(contact.pubkey.as_deref(), Some(them_pk.as_str()));

        assert!(app.send_text("later".into()).is_empty(), "offline: queued");
        assert!(app.status_msg.contains("queued"));
        assert_eq!(
            deliveries(&app, &them_pk),
            vec![
                ("earlier".into(), Delivery::Queued),
                ("later".into(), Delivery::Queued)
            ]
        );

        // The next connection's sync writes both blocks.
        let hashes: Vec<String> = app.history[&them_pk]
            .iter()
            .filter_map(|l| l.chain.as_ref().map(|m| m.hash.clone()))
            .collect();
        for hash in &hashes {
            app.on_event(AppEvent::BlockDelivered {
                id: contact.id.clone(),
                hash: hash.clone(),
            });
        }
        assert!(deliveries(&app, &them_pk)
            .iter()
            .all(|(_, d)| *d == Delivery::Sent));

        // They receive everything and reply on top: our messages are acked.
        let me_pk = me.pubkey_hex();
        for b in my_chains.with(&them_pk, |c| c.missing_for(&[])).unwrap() {
            their_chains.with(&me_pk, |c| c.insert(b)).unwrap().unwrap();
        }
        let reply = their_chains.compose(&me_pk, "got them").unwrap();
        my_chains
            .with(&them_pk, |c| c.insert(reply))
            .unwrap()
            .unwrap();
        app.on_event(AppEvent::ChainChanged {
            id: contact.id.clone(),
            pubkey: them_pk.clone(),
            from_peer: vec!["got them".into()],
        });
        assert_eq!(
            deliveries(&app, &them_pk),
            vec![
                ("earlier".into(), Delivery::Acked),
                ("later".into(), Delivery::Acked),
                ("got them".into(), Delivery::Received)
            ]
        );
    }

    #[test]
    fn online_chain_peer_gets_block_and_legacy_peer_gets_text() {
        let (me, them) = (test_identity(), test_identity());
        let mut app = App::new("me".into(), 7421)
            .with_pubkey(me.pubkey_hex())
            .with_chains(test_chains(&me));
        app.on_event(AppEvent::PeerConnecting {
            id: "p".into(),
            addr: "192.0.2.9:7421".parse().unwrap(),
            dialed: true,
        });
        app.on_event(AppEvent::PeerAuthenticated {
            id: "p".into(),
            pubkey: them.pubkey_hex(),
            username: "them".into(),
        });
        // No chain support announced yet: plain text.
        assert!(matches!(
            &app.send_text("a".into())[..],
            [NetCmd::SendText { .. }]
        ));
        app.on_event(AppEvent::PeerChainReady { id: "p".into() });
        assert!(
            matches!(&app.send_text("b".into())[..], [NetCmd::SendBlock { block, .. }] if block.body == "b")
        );
    }

    #[test]
    fn verify_shows_symmetric_safety_code() {
        let (me, them) = (test_identity(), test_identity());
        let mut app = App::new("me".into(), 7421).with_pubkey(me.pubkey_hex());
        app.run_command("verify");
        assert!(app.status_msg.contains("handshake"), "no peer yet");
        online_peer(&mut app, "p");
        app.peers[0].pubkey = Some(them.pubkey_hex());
        app.run_command("verify");
        let line = app.history[&them.pubkey_hex()].last().unwrap().body.clone();
        let code = chain::safety_code(&them.pubkey_bytes(), &me.pubkey_bytes());
        assert!(line.contains(&code), "{line} should contain {code}");
    }

    fn discover(app: &mut App, id: &str, addr: &str, pk: Option<&str>) {
        app.on_event(AppEvent::PeerDiscovered {
            id: id.into(),
            username: "alice".into(),
            addr: addr.parse().unwrap(),
            pubkey: pk.map(String::from),
        });
    }

    fn inbound_auth(app: &mut App, id: &str, src: &str, pk: &str) {
        app.on_event(AppEvent::PeerConnecting {
            id: id.into(),
            addr: src.parse().unwrap(),
            dialed: false,
        });
        app.on_event(AppEvent::PeerAuthenticated {
            id: id.into(),
            pubkey: pk.into(),
            username: "alice".into(),
        });
    }

    /// Issue #7: offline -> online -> offline -> online must not leave two
    /// "alice" rows behind.
    #[test]
    fn reconnect_cycles_keep_one_row() {
        let mut app = App::new("me".into(), 7421);
        let pk = unique_pubkey();
        let mdns = "mdns:alice@host-7421._cweldrop._tcp.local.";
        for round in 0..3 {
            discover(&mut app, mdns, "192.0.2.7:7421", Some(&pk));
            assert_eq!(app.peers.len(), 1, "round {round}: discovery");
            let inbound = format!("inbound:192.0.2.7:{}", 40000 + round);
            inbound_auth(
                &mut app,
                &inbound,
                &format!("192.0.2.7:{}", 40000 + round),
                &pk,
            );
            assert_eq!(app.peers.len(), 1, "round {round}: connected");
            assert_eq!(app.peers[0].status, PeerStatus::Online);
            app.on_event(AppEvent::PeerDisconnected {
                id: inbound.clone(),
            });
            // The row is dialable at the listening port, not the source port.
            assert_eq!(app.peers[0].status, PeerStatus::Available);
            assert_eq!(app.peers[0].addr, Some("192.0.2.7:7421".parse().unwrap()));
            app.on_event(AppEvent::PeerLost { id: mdns.into() });
            assert_eq!(app.peers.len(), 1, "round {round}: offline");
            assert_eq!(app.peers[0].status, PeerStatus::Offline);
        }
    }

    /// The announcement and the connection come from different addresses
    /// (IPv6 vs IPv4): the key still ties them together.
    #[test]
    fn same_key_different_ip_merges() {
        let mut app = App::new("me".into(), 7421);
        let pk = unique_pubkey();
        discover(&mut app, "mdns:a", "[2001:db8::7]:7421", Some(&pk));
        inbound_auth(&mut app, "inbound:192.0.2.7:4000", "192.0.2.7:4000", &pk);
        assert_eq!(app.peers.len(), 1);
        // And a late announcement for the connected key attaches too.
        discover(&mut app, "mdns:b", "192.0.2.8:7421", Some(&pk));
        assert_eq!(app.peers.len(), 1);
        assert_eq!(app.peers[0].status, PeerStatus::Online);
    }

    #[test]
    fn two_users_on_one_host_stay_separate() {
        let mut app = App::new("me".into(), 7421);
        let (pk_a, pk_b) = (unique_pubkey(), unique_pubkey());
        discover(&mut app, "mdns:a", "192.0.2.7:7421", Some(&pk_a));
        discover(&mut app, "mdns:b", "192.0.2.7:7422", Some(&pk_b));
        inbound_auth(&mut app, "inbound:192.0.2.7:4000", "192.0.2.7:4000", &pk_a);
        assert_eq!(app.peers.len(), 2, "b's row isn't swallowed by IP match");
        assert!(app
            .peers
            .iter()
            .any(|p| p.claimed_pubkey.as_deref() == Some(pk_b.as_str())));
    }

    #[test]
    fn discovery_attaches_to_offline_contact() {
        let (me, them) = (test_identity(), test_identity());
        let chains = test_chains(&me);
        chains.compose(&them.pubkey_hex(), "queued").unwrap();
        let mut app = App::new("me".into(), 7421).with_chains(chains);
        assert_eq!(app.peers.len(), 1);
        discover(
            &mut app,
            "mdns:x",
            "192.0.2.7:7421",
            Some(&them.pubkey_hex()),
        );
        assert_eq!(app.peers.len(), 1, "no second row for a known contact");
        assert_eq!(app.peers[0].status, PeerStatus::Available);
        assert!(app.peers[0].addr.is_some(), "contact becomes dialable");
    }

    #[test]
    fn selection_follows_merged_row() {
        let mut app = App::new("me".into(), 7421);
        let pk = unique_pubkey();
        discover(
            &mut app,
            "mdns:other",
            "192.0.2.9:7421",
            Some(&unique_pubkey()),
        );
        discover(&mut app, "mdns:a", "192.0.2.7:7421", Some(&pk));
        app.selected = 1;
        inbound_auth(&mut app, "inbound:192.0.2.7:4000", "192.0.2.7:4000", &pk);
        assert_eq!(
            app.selected_peer().unwrap().pubkey.as_deref(),
            Some(pk.as_str())
        );
    }

    /// Issue #8: a row stands for a key; a different key can't take it over.
    #[test]
    fn key_mismatch_is_refused() {
        let mut app = App::new("me".into(), 7421);
        let (real, fake) = (unique_pubkey(), unique_pubkey());
        discover(&mut app, "mdns:alice", "192.0.2.7:7421", Some(&real));
        // We dial the announced alice, but whoever answers proves another key.
        app.on_event(AppEvent::PeerConnecting {
            id: "mdns:alice".into(),
            addr: "192.0.2.7:7421".parse().unwrap(),
            dialed: true,
        });
        app.on_event(AppEvent::PeerAuthenticated {
            id: "mdns:alice".into(),
            pubkey: fake.clone(),
            username: "alice".into(),
        });
        let p = &app.peers[0];
        assert_eq!(p.status, PeerStatus::Error);
        assert!(p.last_error.as_deref().unwrap().contains("key mismatch"));
        assert_eq!(p.pubkey, None, "fake key not adopted");
        assert!(matches!(&app.outbox[..], [NetCmd::Disconnect { id }] if id == "mdns:alice"));
        assert!(!app.history.contains_key(&fake));
    }

    /// Issue #8: same nickname, different key than a known contact.
    #[test]
    fn impostor_using_known_name_is_flagged() {
        let (me, alice) = (test_identity(), test_identity());
        let chains = test_chains(&me);
        chains.set_name(&alice.pubkey_hex(), "alice").unwrap();
        let mut app = App::new("me".into(), 7421).with_chains(chains);
        let mallory = unique_pubkey();
        inbound_auth(
            &mut app,
            "inbound:192.0.2.66:5000",
            "192.0.2.66:5000",
            &mallory,
        );
        let p = app
            .peers
            .iter()
            .find(|p| p.pubkey.as_deref() == Some(mallory.as_str()))
            .unwrap();
        let w = p.warning.as_deref().expect("flagged");
        assert!(w.contains("not the alice you know"), "{w}");

        // The real alice is not flagged.
        let mut app2 = App::new("me".into(), 7421).with_chains(test_chains(&me));
        inbound_auth(
            &mut app2,
            "inbound:x",
            "192.0.2.7:5000",
            &alice.pubkey_hex(),
        );
        assert!(app2.peers.iter().all(|p| p.warning.is_none()));
    }

    #[test]
    fn verify_confirm_persists_badge() {
        let (me, them) = (test_identity(), test_identity());
        let chains = test_chains(&me);
        chains.compose(&them.pubkey_hex(), "hi").unwrap();
        let mut app = App::new("me".into(), 7421)
            .with_pubkey(me.pubkey_hex())
            .with_chains(Arc::clone(&chains));
        assert!(!app.peers[0].verified);
        app.run_command("verify confirm");
        assert!(app.peers[0].verified);
        assert!(chains.is_verified(&them.pubkey_hex()));
        // Reloaded contacts keep the badge.
        let again = App::new("me".into(), 7421).with_chains(Arc::clone(&chains));
        assert!(again.peers[0].verified);
        app.run_command("verify forget");
        assert!(!app.peers[0].verified && !chains.is_verified(&them.pubkey_hex()));
        app.run_command("verify bogus");
        assert!(app.status_msg.contains("usage"));
    }

    fn offer_named(app: &mut App, file_id: &str, name: &str) {
        app.on_event(AppEvent::FileOffered {
            id: "p".into(),
            file_id: file_id.into(),
            name: name.into(),
            size: 10,
        });
    }

    /// Issue #11: with several offers, a bare :accept asks which one.
    #[test]
    fn several_offers_open_the_files_panel() {
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");
        offer_named(&mut app, "f1", "report.pdf");
        offer_named(&mut app, "f2", "cat.png");
        offer_named(&mut app, "f3", "notes.txt");
        app.focus = Focus::Command;
        assert!(app.run_command("accept").is_empty());
        assert_eq!(app.focus, Focus::Files, "ambiguous: let the user pick");
        assert!(app.status_msg.contains("3 pending offers"));

        // Newest first: notes.txt, cat.png, report.pdf. Move to cat.png.
        app.files_action(FilesAction::Down);
        let cmds = app.files_action(FilesAction::Reject);
        assert!(matches!(&cmds[..], [NetCmd::RejectFile { file_id, .. }] if file_id == "f2"));
        let cmds = app.files_action(FilesAction::Open); // still on cat.png: declined
        assert!(cmds.is_empty());
        let cmds = app.files_action(FilesAction::AcceptAll);
        assert_eq!(cmds.len(), 2, "remaining two pending offers");
        assert_eq!(state_of(&app, "f1"), FileState::Transferring(0));
        assert_eq!(state_of(&app, "f2"), FileState::Rejected);
        assert_eq!(state_of(&app, "f3"), FileState::Transferring(0));
        app.files_action(FilesAction::Close);
        assert_eq!(app.focus, Focus::Input);
    }

    #[test]
    fn accept_and_reject_by_name_number_or_all() {
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");
        offer_named(&mut app, "f1", "report.pdf");
        offer_named(&mut app, "f2", "cat.png");
        offer_named(&mut app, "f3", "cat-2.png");
        let cmds = app.run_command("accept report");
        assert!(matches!(&cmds[..], [NetCmd::AcceptFile { file_id, .. }] if file_id == "f1"));
        let cmds = app.run_command("accept CAT.PNG");
        assert!(matches!(&cmds[..], [NetCmd::AcceptFile { file_id, .. }] if file_id == "f2"));
        assert!(app.run_command("reject nope").is_empty());
        assert!(app.status_msg.contains("no offer matches"));
        assert_eq!(app.run_command("reject all").len(), 1);
        assert_eq!(state_of(&app, "f3"), FileState::Rejected);
    }

    #[test]
    fn view_by_name_and_panel_opens_files() {
        let mut app = App::new("me".into(), 7421);
        online_peer(&mut app, "p");
        for (id, name) in [("f1", "a.txt"), ("f2", "b.png")] {
            offer_named(&mut app, id, name);
            app.on_event(AppEvent::FileReceived {
                id: "p".into(),
                file_id: id.into(),
                path: format!("/tmp/{name}").into(),
            });
        }
        app.run_command("view");
        assert_eq!(app.focus, Focus::Files, "two files: pick");
        assert!(app.viewer.is_none());
        app.files_action(FilesAction::Down); // a.txt (older)
        app.files_action(FilesAction::Open);
        assert_eq!(app.pending_view.take(), Some("/tmp/a.txt".into()));
        assert_eq!(app.focus, Focus::Input);
        app.close_viewer();
        app.run_command("view b.png");
        assert_eq!(app.pending_view.take(), Some("/tmp/b.png".into()));
    }

    #[test]
    fn pick_resolution() {
        let f = |name: &str| FileInfo {
            file_id: String::new(),
            name: name.into(),
            size: 1,
            path: None,
            state: FileState::Done,
        };
        let items = vec![f("notes.txt"), f("cat.png"), f("cat-2.png")];
        assert_eq!(pick(&items, ""), Pick::Ambiguous);
        assert_eq!(pick(&items[..1], ""), Pick::One(0));
        assert_eq!(pick(&items, "2"), Pick::One(1));
        assert_eq!(pick(&items, "9"), Pick::Nothing);
        assert_eq!(pick(&items, "cat.png"), Pick::One(1), "exact beats partial");
        assert_eq!(pick(&items, "cat"), Pick::Ambiguous);
        assert_eq!(pick(&items, "notes"), Pick::One(0));
        assert_eq!(pick(&items, "ALL"), Pick::All);
        assert_eq!(pick(&[], ""), Pick::Nothing);
    }

    fn typed(app: &mut App, text: &str) {
        app.focus = Focus::Command;
        app.command = text.into();
        app.command_cursor = text.len();
        app.cmd_sel = 0;
    }

    /// Issue #10: the hint panel's fuzzy pick runs or completes commands.
    #[test]
    fn command_panel_enter_runs_or_completes() {
        let mut app = App::new("me".into(), 7421);
        typed(&mut app, "stg");
        app.command_enter();
        assert_eq!(app.focus, Focus::Settings, "fuzzy 'stg' ran :settings");

        typed(&mut app, "con");
        assert!(app.command_enter().is_empty());
        assert_eq!(app.focus, Focus::Command, "connect needs an address");
        assert_eq!(app.command, "connect ");
        assert_eq!(app.command_signature().unwrap().name, "connect");
        app.command.push_str("192.0.2.1:7421");
        let cmds = app.command_enter();
        assert!(matches!(&cmds[..], [NetCmd::Connect { .. }]));
        assert_eq!(app.focus, Focus::Input);

        // An exact command runs as typed, even if a suggestion is highlighted.
        typed(&mut app, "q");
        app.command_enter();
        assert!(app.should_quit);
    }

    #[test]
    fn command_panel_tab_and_arrows() {
        let mut app = App::new("me".into(), 7421);
        typed(&mut app, "");
        let all = app.command_suggestions().len();
        assert_eq!(all, crate::commands::COMMANDS.len());
        app.command_move(-1);
        assert_eq!(app.cmd_sel, all - 1, "wraps upward");
        app.command_move(1);
        assert_eq!(app.cmd_sel, 0);

        typed(&mut app, "fil");
        assert!(app.command_complete());
        assert_eq!(app.command, "files", "no args: no trailing space");
        typed(&mut app, "sen");
        app.command_complete();
        assert_eq!(app.command, "send ");
        assert!(app.command_suggestions().is_empty(), "signature mode");

        app.run_command("help");
        assert_eq!(app.focus, Focus::Command);
        assert!(app.command.is_empty());
    }

    #[test]
    fn pdf_viewer_flips_pages_and_ignores_stale_ones() {
        let mut app = App::new("me".into(), 7421);
        app.open_viewer("/tmp/doc.pdf".into());
        app.pending_view = None;
        let page = |n| {
            let picker = ratatui_image::picker::Picker::from_fontsize((10, 20));
            Content::Image {
                proto: crate::event::ImageProto(
                    picker.new_resize_protocol(image::DynamicImage::new_rgb8(1, 1)),
                ),
                page: Some((n, 3)),
            }
        };
        app.on_event(AppEvent::ViewerLoaded {
            path: "/tmp/doc.pdf".into(),
            page: 1,
            result: Ok(page(1)),
        });
        assert!(
            !app.viewer_page(-1) || app.pending_view.is_none(),
            "already first"
        );
        assert!(app.viewer_page(1));
        assert_eq!(app.viewer.as_ref().unwrap().page, 2);
        assert_eq!(app.pending_view, Some("/tmp/doc.pdf".into()));
        assert!(app.viewer.as_ref().unwrap().content.is_none(), "loading");
        // A late page-1 result must not replace the page being loaded.
        app.on_event(AppEvent::ViewerLoaded {
            path: "/tmp/doc.pdf".into(),
            page: 1,
            result: Ok(page(1)),
        });
        assert!(app.viewer.as_ref().unwrap().content.is_none());
        app.on_event(AppEvent::ViewerLoaded {
            path: "/tmp/doc.pdf".into(),
            page: 2,
            result: Ok(page(2)),
        });
        assert!(app.viewer.as_ref().unwrap().content.is_some());
        // Text has no pages: the key falls back to scrolling.
        app.viewer.as_mut().unwrap().content = Some(Content::Text(Default::default()));
        assert!(!app.viewer_page(1));
    }

    #[test]
    fn click_hits_only_link_cells() {
        let mut app = App::new("me".into(), 7421);
        app.link_regions = vec![LinkRegion {
            y: 3,
            x_start: 10,
            x_end: 20,
            target: LinkTarget::Url("https://a.com".into()),
        }];
        app.click(9, 3);
        app.click(20, 3);
        app.click(15, 4);
        assert_eq!(app.pending_open, None);
        app.click(10, 3);
        assert_eq!(app.pending_open.take().as_deref(), Some("https://a.com"));
        app.click(19, 3);
        assert_eq!(app.pending_open.as_deref(), Some("https://a.com"));
    }
}
