//! Per-connection file transfer state machine.
//!
//! Lives inside a peer task. Outgoing files are read by a spawned task that
//! feeds `file_chunk` frames into a small bounded channel, which the peer loop
//! drains alongside chat traffic, so a big transfer never blocks messages.
//! Incoming files are written to a hidden `.part` file in the target folder,
//! size-capped by the offer, SHA-256 verified, and only then renamed into
//! place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::warn;

use super::protocol::Message;
use super::PeerId;
use crate::event::AppEvent;
use crate::files;
use crate::history_store::{self, Direction, StoredFile, StoredLine};

/// Emit a progress event at most every this many bytes.
const PROGRESS_STEP: u64 = 256 * 1024;
/// Cap on offers awaiting a decision, so a peer can't grow memory unbounded.
const MAX_PENDING_OFFERS: usize = 32;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

struct Outgoing {
    path: PathBuf,
    name: String,
    size: u64,
    /// Set once the reader task is running.
    task: Option<AbortHandle>,
}

struct Incoming {
    file: tokio::fs::File,
    tmp: PathBuf,
    dir: PathBuf,
    name: String,
    size: u64,
    received: u64,
    hasher: Sha256,
    last_progress: u64,
}

pub struct Transfers {
    peer_id: PeerId,
    self_username: String,
    remote_username: String,
    /// Verified remote pubkey; completed transfers are logged to its history.
    pubkey: Option<String>,
    evt_tx: mpsc::Sender<AppEvent>,
    /// Feeds reader-task frames back to the peer loop.
    chunk_tx: mpsc::Sender<Message>,
    outgoing: HashMap<String, Outgoing>,
    offers: HashMap<String, (String, u64)>,
    incoming: HashMap<String, Incoming>,
}

impl Transfers {
    pub fn new(
        peer_id: PeerId,
        self_username: String,
        remote_username: String,
        pubkey: Option<String>,
        evt_tx: mpsc::Sender<AppEvent>,
        chunk_tx: mpsc::Sender<Message>,
    ) -> Self {
        Self {
            peer_id,
            self_username,
            remote_username,
            pubkey,
            evt_tx,
            chunk_tx,
            outgoing: HashMap::new(),
            offers: HashMap::new(),
            incoming: HashMap::new(),
        }
    }

    async fn emit(&self, ev: AppEvent) {
        let _ = self.evt_tx.send(ev).await;
    }

    async fn fail(&self, file_id: &str, error: impl Into<String>) {
        self.emit(AppEvent::FileFailed {
            id: self.peer_id.clone(),
            file_id: file_id.to_string(),
            error: error.into(),
        })
        .await;
    }

    // ---- local commands (return frames to write) ----

    /// Offer `path` to the remote.
    pub fn offer(&mut self, file_id: String, path: PathBuf, name: String, size: u64) -> Message {
        let msg = Message::FileOffer {
            file_id: file_id.clone(),
            name: name.clone(),
            size,
        };
        self.outgoing.insert(
            file_id,
            Outgoing {
                path,
                name,
                size,
                task: None,
            },
        );
        msg
    }

    /// Accept a pending offer, saving into `dir`.
    pub async fn accept(&mut self, file_id: &str, dir: &Path) -> Option<Message> {
        let (name, size) = self.offers.remove(file_id)?;
        match open_part(dir, file_id).await {
            Ok((file, tmp)) => {
                self.incoming.insert(
                    file_id.to_string(),
                    Incoming {
                        file,
                        tmp,
                        dir: dir.to_path_buf(),
                        name,
                        size,
                        received: 0,
                        hasher: Sha256::new(),
                        last_progress: 0,
                    },
                );
                Some(Message::FileAccept {
                    file_id: file_id.to_string(),
                })
            }
            Err(e) => {
                self.fail(file_id, format!("cannot write to {}: {e}", dir.display()))
                    .await;
                Some(Message::FileReject {
                    file_id: file_id.to_string(),
                })
            }
        }
    }

    pub fn reject(&mut self, file_id: &str) -> Option<Message> {
        self.offers.remove(file_id).map(|_| Message::FileReject {
            file_id: file_id.to_string(),
        })
    }

    // ---- remote frames (return frames to write in reply) ----

    pub async fn on_offer(&mut self, file_id: String, name: String, size: u64) -> Option<Message> {
        if !files::valid_file_id(&file_id) || self.offers.contains_key(&file_id) {
            warn!(%file_id, "ignoring bad or duplicate file offer");
            return None;
        }
        if self.offers.len() >= MAX_PENDING_OFFERS {
            return Some(Message::FileReject { file_id });
        }
        let name = files::sanitize_name(&name);
        self.offers.insert(file_id.clone(), (name.clone(), size));
        self.emit(AppEvent::FileOffered {
            id: self.peer_id.clone(),
            file_id,
            name,
            size,
        })
        .await;
        None
    }

    pub async fn on_accept(&mut self, file_id: String) {
        let Some(out) = self.outgoing.get_mut(&file_id) else {
            return;
        };
        if out.task.is_some() {
            return;
        }
        let task = tokio::spawn(read_and_send(
            file_id.clone(),
            out.path.clone(),
            out.size,
            self.chunk_tx.clone(),
            self.evt_tx.clone(),
            self.peer_id.clone(),
        ));
        out.task = Some(task.abort_handle());
        self.emit(AppEvent::FileAccepted {
            id: self.peer_id.clone(),
            file_id,
        })
        .await;
    }

    pub async fn on_reject(&mut self, file_id: String) {
        if self.outgoing.remove(&file_id).is_some() {
            self.emit(AppEvent::FileRejected {
                id: self.peer_id.clone(),
                file_id,
            })
            .await;
        }
    }

    pub async fn on_chunk(&mut self, file_id: String, data: String) -> Option<Message> {
        let inc = self.incoming.get_mut(&file_id)?;
        let err = match B64.decode(data.as_bytes()) {
            Err(e) => Some(format!("bad chunk encoding: {e}")),
            Ok(bytes) if inc.received + bytes.len() as u64 > inc.size => {
                Some("sender exceeded the offered size".to_string())
            }
            Ok(bytes) => match inc.file.write_all(&bytes).await {
                Err(e) => Some(format!("write failed: {e}")),
                Ok(()) => {
                    inc.hasher.update(&bytes);
                    inc.received += bytes.len() as u64;
                    if inc.received - inc.last_progress >= PROGRESS_STEP {
                        inc.last_progress = inc.received;
                        let bytes = inc.received;
                        self.emit(AppEvent::FileProgress {
                            id: self.peer_id.clone(),
                            file_id: file_id.clone(),
                            bytes,
                        })
                        .await;
                    }
                    None
                }
            },
        };
        let error = err?;
        self.drop_incoming(&file_id).await;
        self.fail(&file_id, error).await;
        Some(Message::FileCancel { file_id })
    }

    pub async fn on_done(&mut self, file_id: String, sha256: String) {
        let Some(mut inc) = self.incoming.remove(&file_id) else {
            return;
        };
        let digest = hex::encode(std::mem::take(&mut inc.hasher).finalize());
        let error = if inc.received != inc.size {
            Some(format!("got {} of {} bytes", inc.received, inc.size))
        } else if !digest.eq_ignore_ascii_case(&sha256) {
            Some("checksum mismatch".to_string())
        } else if let Err(e) = inc.file.flush().await {
            Some(format!("flush failed: {e}"))
        } else {
            None
        };
        if let Some(e) = error {
            let _ = tokio::fs::remove_file(&inc.tmp).await;
            return self.fail(&file_id, e).await;
        }
        drop(inc.file);
        let dest = files::unique_path(&inc.dir, &inc.name);
        if let Err(e) = tokio::fs::rename(&inc.tmp, &dest).await {
            let _ = tokio::fs::remove_file(&inc.tmp).await;
            return self.fail(&file_id, format!("save failed: {e}")).await;
        }
        self.log_history(Direction::Recv, &inc.name, inc.size, &dest);
        self.emit(AppEvent::FileReceived {
            id: self.peer_id.clone(),
            file_id,
            path: dest,
        })
        .await;
    }

    pub async fn on_cancel(&mut self, file_id: String) {
        if self.offers.remove(&file_id).is_some() {
            self.fail(&file_id, "offer withdrawn").await;
        } else if self.incoming.contains_key(&file_id) {
            self.drop_incoming(&file_id).await;
            self.fail(&file_id, "cancelled by sender").await;
        } else if let Some(out) = self.outgoing.remove(&file_id) {
            if let Some(t) = out.task {
                t.abort();
            }
            self.fail(&file_id, "cancelled by receiver").await;
        }
    }

    /// The peer loop just wrote one of our reader-task frames.
    pub async fn on_frame_written(&mut self, msg: &Message) {
        match msg {
            Message::FileDone { file_id, .. } => {
                if let Some(out) = self.outgoing.remove(file_id) {
                    self.log_history(Direction::Sent, &out.name, out.size, &out.path);
                    self.emit(AppEvent::FileSent {
                        id: self.peer_id.clone(),
                        file_id: file_id.clone(),
                    })
                    .await;
                }
            }
            Message::FileCancel { file_id } => {
                // Reader task gave up (e.g. file unreadable); it already
                // reported the reason.
                self.outgoing.remove(file_id);
            }
            _ => {}
        }
    }

    /// Connection is going away: abort reads, delete partial files, and tell
    /// the UI about everything that didn't finish.
    pub async fn shutdown(&mut self) {
        for (id, out) in std::mem::take(&mut self.outgoing) {
            if let Some(t) = out.task {
                t.abort();
            }
            self.fail(&id, "connection closed").await;
        }
        let ids: Vec<String> = self.incoming.keys().cloned().collect();
        for id in ids {
            self.drop_incoming(&id).await;
            self.fail(&id, "connection closed").await;
        }
        for id in std::mem::take(&mut self.offers).into_keys() {
            self.fail(&id, "connection closed").await;
        }
    }

    async fn drop_incoming(&mut self, file_id: &str) {
        if let Some(inc) = self.incoming.remove(file_id) {
            drop(inc.file);
            let _ = tokio::fs::remove_file(&inc.tmp).await;
        }
    }

    fn log_history(&self, dir: Direction, name: &str, size: u64, path: &Path) {
        let Some(pk) = self.pubkey.as_ref() else {
            return;
        };
        let from = match dir {
            Direction::Sent => self.self_username.clone(),
            Direction::Recv => self.remote_username.clone(),
        };
        let line = StoredLine {
            dir,
            from,
            body: name.to_string(),
            at: Utc::now(),
            file: Some(StoredFile {
                name: name.to_string(),
                size,
                path: path.to_path_buf(),
            }),
        };
        if let Err(e) = history_store::append(pk, &line) {
            warn!(error = %e, "history append (file) failed");
        }
    }
}

async fn open_part(dir: &Path, file_id: &str) -> std::io::Result<(tokio::fs::File, PathBuf)> {
    tokio::fs::create_dir_all(dir).await?;
    let tmp = dir.join(format!(".cweldrop-{file_id}.part"));
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await?;
    Ok((file, tmp))
}

/// Stream `path` as chunk frames into `chunk_tx`, then `file_done`.
async fn read_and_send(
    file_id: String,
    path: PathBuf,
    size: u64,
    chunk_tx: mpsc::Sender<Message>,
    evt_tx: mpsc::Sender<AppEvent>,
    peer_id: PeerId,
) {
    let fail = |error: String| {
        let (chunk_tx, evt_tx, peer_id, file_id) = (
            chunk_tx.clone(),
            evt_tx.clone(),
            peer_id.clone(),
            file_id.clone(),
        );
        async move {
            let _ = chunk_tx
                .send(Message::FileCancel {
                    file_id: file_id.clone(),
                })
                .await;
            let _ = evt_tx
                .send(AppEvent::FileFailed {
                    id: peer_id,
                    file_id,
                    error,
                })
                .await;
        }
    };
    let mut f = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return fail(format!("open {}: {e}", path.display())).await,
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; files::CHUNK_SIZE];
    let mut sent = 0u64;
    let mut last_progress = 0u64;
    while sent < size {
        let want = (size - sent).min(buf.len() as u64) as usize;
        let n = match f.read(&mut buf[..want]).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return fail(format!("read failed: {e}")).await,
        };
        hasher.update(&buf[..n]);
        sent += n as u64;
        let chunk = Message::FileChunk {
            file_id: file_id.clone(),
            data: B64.encode(&buf[..n]),
        };
        if chunk_tx.send(chunk).await.is_err() {
            return; // connection gone; shutdown() reports it
        }
        if sent - last_progress >= PROGRESS_STEP {
            last_progress = sent;
            let _ = evt_tx
                .send(AppEvent::FileProgress {
                    id: peer_id.clone(),
                    file_id: file_id.clone(),
                    bytes: sent,
                })
                .await;
        }
    }
    if sent != size {
        return fail("file changed while sending".into()).await;
    }
    let _ = chunk_tx
        .send(Message::FileDone {
            file_id,
            sha256: hex::encode(hasher.finalize()),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Transfers, mpsc::Receiver<AppEvent>, PathBuf) {
        let (evt_tx, evt_rx) = mpsc::channel(64);
        let (chunk_tx, _chunk_rx) = mpsc::channel(4);
        let t = Transfers::new(
            "p".into(),
            "me".into(),
            "them".into(),
            None,
            evt_tx,
            chunk_tx,
        );
        let dir = std::env::temp_dir().join(format!("cweldrop-xfer-{}", rand::random::<u64>()));
        (t, evt_rx, dir)
    }

    fn entries(dir: &Path) -> usize {
        std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
    }

    #[tokio::test]
    async fn chunk_beyond_offered_size_cancels() {
        let (mut t, mut evt, dir) = setup();
        assert!(t.on_offer("aa".into(), "x.bin".into(), 4).await.is_none());
        assert!(matches!(
            evt.recv().await,
            Some(AppEvent::FileOffered { .. })
        ));
        assert!(matches!(
            t.accept("aa", &dir).await,
            Some(Message::FileAccept { .. })
        ));
        assert_eq!(entries(&dir), 1, ".part file created");
        let reply = t.on_chunk("aa".into(), B64.encode([0u8; 5])).await;
        assert_eq!(
            reply,
            Some(Message::FileCancel {
                file_id: "aa".into()
            })
        );
        assert!(matches!(
            evt.recv().await,
            Some(AppEvent::FileFailed { .. })
        ));
        assert_eq!(entries(&dir), 0, ".part file removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn checksum_mismatch_discards_file() {
        let (mut t, mut evt, dir) = setup();
        t.on_offer("bb".into(), "x.bin".into(), 3).await;
        let _ = evt.recv().await;
        t.accept("bb", &dir).await;
        assert!(t.on_chunk("bb".into(), B64.encode(b"abc")).await.is_none());
        t.on_done("bb".into(), "00".repeat(32)).await;
        match evt.recv().await {
            Some(AppEvent::FileFailed { error, .. }) => assert!(error.contains("checksum")),
            other => panic!("expected failure, got {other:?}"),
        }
        assert_eq!(entries(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn verified_file_is_saved_under_sanitised_name() {
        let (mut t, mut evt, dir) = setup();
        t.on_offer("cc".into(), "../../etc/x.txt".into(), 3).await;
        match evt.recv().await {
            Some(AppEvent::FileOffered { name, .. }) => assert_eq!(name, "x.txt"),
            other => panic!("{other:?}"),
        }
        t.accept("cc", &dir).await;
        t.on_chunk("cc".into(), B64.encode(b"abc")).await;
        t.on_done("cc".into(), hex::encode(Sha256::digest(b"abc")))
            .await;
        match evt.recv().await {
            Some(AppEvent::FileReceived { path, .. }) => {
                assert_eq!(path, dir.join("x.txt"));
                assert_eq!(std::fs::read(&path).unwrap(), b"abc");
            }
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bad_ids_and_unknown_transfers_are_ignored() {
        let (mut t, mut evt, dir) = setup();
        assert!(t.on_offer("../evil".into(), "x".into(), 1).await.is_none());
        assert!(t.on_chunk("nope".into(), "AA==".into()).await.is_none());
        t.on_done("nope".into(), String::new()).await;
        assert!(t.accept("nope", &dir).await.is_none());
        assert!(evt.try_recv().is_err(), "no events for junk");
        assert!(!dir.exists(), "nothing written");
    }
}
