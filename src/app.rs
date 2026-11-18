use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};

use crate::event::{AppEvent, ImageProto};
use crate::files;
use crate::history_store;
use crate::net::{NetCmd, PeerId};
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
}

/// How long a "typing…" indicator stays visible after the last ping. The
/// sender re-pings roughly once a second while editing, so this only lapses
/// once they actually stop.
pub const TYPING_TTL: Duration = Duration::from_secs(5);

impl Peer {
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
}

impl ChatLine {
    pub fn text(from: String, body: String) -> Self {
        Self {
            from,
            body,
            at: SystemTime::now(),
            file: None,
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
}

/// Rows of the settings panel, in display order.
pub const SETTINGS_ROWS: usize = 3;

/// Full-screen image viewer state.
#[derive(Debug)]
pub struct Viewer {
    pub name: String,
    pub path: PathBuf,
    /// `None` while decoding.
    pub image: Option<ImageProto>,
    pub error: Option<String>,
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
    /// NetCmds produced while handling events; drained by the main loop.
    pub outbox: Vec<NetCmd>,
    pub viewer: Option<Viewer>,
    /// An image the main loop should decode for the viewer.
    pub pending_view: Option<PathBuf>,
    /// Ask the main loop for a full terminal clear (after graphics overlays).
    pub needs_clear: bool,
}

/// What a clickable region in the chat leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Url(String),
    Image(PathBuf),
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
            outbox: Vec::new(),
            viewer: None,
            pending_view: None,
            needs_clear: false,
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

    fn history_key_for_id(&self, id: &str) -> String {
        self.peer_index(id)
            .map(|i| self.peers[i].history_key().to_string())
            .unwrap_or_else(|| id.to_string())
    }

    /// Returns a NetCmd if the app event implies one. Mostly events are absorbed into state.
    pub fn on_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::PeerDiscovered { id, username, addr } => {
                if let Some(idx) = self.peer_index(&id) {
                    let p = &mut self.peers[idx];
                    p.username = username;
                    p.addr = Some(addr);
                    if matches!(p.status, PeerStatus::Offline | PeerStatus::Error) {
                        p.status = PeerStatus::Available;
                    }
                } else {
                    self.peers.push(Peer {
                        id,
                        username,
                        addr: Some(addr),
                        status: PeerStatus::Available,
                        last_error: None,
                        pubkey: None,
                        typing_at: None,
                    });
                }
            }
            AppEvent::PeerLost { id } => {
                if let Some(idx) = self.peer_index(&id) {
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
                    self.peers.push(Peer {
                        id,
                        username,
                        addr: Some(addr),
                        status: PeerStatus::Connecting,
                        last_error: None,
                        pubkey: None,
                        typing_at: None,
                    });
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
                    let conn_id = self.peers[idx].id.clone();
                    let auth_ip = self.peers[idx].addr.map(|a| a.ip());
                    self.peers[idx].pubkey = Some(pubkey.clone());
                    if !username.is_empty() {
                        self.peers[idx].username = username;
                    }
                    self.peers[idx].status = PeerStatus::Online;

                    // Load persisted history once, on first authentication.
                    if !self.history.contains_key(&pubkey) {
                        match history_store::load(&pubkey) {
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
                                    })
                                    .collect();
                                self.history.insert(pubkey.clone(), loaded);
                            }
                            Err(e) => {
                                tracing::warn!(error=%e, %pubkey, "history load failed");
                                self.history.entry(pubkey.clone()).or_default();
                            }
                        }
                    }

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
                    let dup_ids: Vec<String> = self
                        .peers
                        .iter()
                        .filter(|p| p.id != auth_id)
                        .filter(|p| {
                            p.pubkey.as_deref() == Some(pubkey.as_str())
                                || (p.pubkey.is_none()
                                    && auth_ip.is_some()
                                    && p.addr.map(|a| a.ip()) == auth_ip)
                        })
                        .map(|p| p.id.clone())
                        .collect();
                    for did in dup_ids {
                        if let Some(pre) = self.history.remove(&did) {
                            self.history.entry(pubkey.clone()).or_default().extend(pre);
                        }
                        if let Some(pos) = self.peer_index(&did) {
                            self.peers.remove(pos);
                        }
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
                let viewing = self.terminal_focused
                    && self
                        .selected_peer()
                        .is_some_and(|p| p.history_key() == key.as_str());
                if !viewing {
                    crate::notify::message(&from, &body);
                }
                self.history
                    .entry(key)
                    .or_default()
                    .push(ChatLine::text(from, body));
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
            AppEvent::ImageDecoded { path, result } => {
                if let Some(v) = self.viewer.as_mut().filter(|v| v.path == path) {
                    match result {
                        Ok(img) => v.image = Some(img),
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
            Some(LinkTarget::Image(path)) => self.open_viewer(path),
            None => {}
        }
    }

    /// Show `path` in the image viewer; the main loop decodes it.
    pub fn open_viewer(&mut self, path: PathBuf) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.viewer = Some(Viewer {
            name,
            path: path.clone(),
            image: None,
            error: None,
        });
        self.pending_view = Some(path);
    }

    pub fn close_viewer(&mut self) {
        if self.viewer.take().is_some() {
            self.needs_clear = true;
        }
    }

    /// Received or sent images in the selected chat that exist on disk,
    /// oldest first, as (display name, path).
    pub fn selected_chat_images(&self) -> Vec<(String, PathBuf)> {
        self.selected_chat()
            .iter()
            .filter_map(|cl| cl.file.as_ref())
            .filter(|f| f.state == FileState::Done && files::is_image(&f.name))
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
    fn pending_offers(&self) -> Vec<String> {
        self.selected_chat()
            .iter()
            .filter_map(|cl| cl.file.as_ref())
            .filter(|f| f.state == FileState::AwaitingDecision)
            .map(|f| f.file_id.clone())
            .collect()
    }

    /// Answer the `n`-th newest pending offer (1 = newest) in this chat.
    fn decide_offer(&mut self, arg: &str, accept: bool) -> Vec<NetCmd> {
        let offers = self.pending_offers();
        let n = if arg.is_empty() {
            Ok(1)
        } else {
            arg.parse::<usize>()
        };
        let file_id = match n {
            Ok(n) if n >= 1 && n <= offers.len() => offers[offers.len() - n].clone(),
            Ok(_) if offers.is_empty() => {
                self.status_msg = "no pending file offers in this chat".into();
                return Vec::new();
            }
            Ok(_) => {
                self.status_msg = format!("only {} pending offer(s)", offers.len());
                return Vec::new();
            }
            Err(_) => {
                self.status_msg = format!("bad offer number: {arg}");
                return Vec::new();
            }
        };
        let Some(id) = self.selected_peer().map(|p| p.id.clone()) else {
            return Vec::new();
        };
        if accept {
            self.set_file_state(&file_id, FileState::Transferring(0));
            let dir = self.settings.download_dir.clone();
            self.status_msg = format!("receiving into {}", dir.display());
            vec![NetCmd::AcceptFile { id, file_id, dir }]
        } else {
            self.set_file_state(&file_id, FileState::Rejected);
            self.status_msg = "file declined".into();
            vec![NetCmd::RejectFile { id, file_id }]
        }
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
                let imgs = self.selected_chat_images();
                let n = if arg.is_empty() {
                    Ok(1)
                } else {
                    arg.parse::<usize>()
                };
                match n {
                    Ok(n) if n >= 1 && n <= imgs.len() => {
                        let path = imgs[imgs.len() - n].1.clone();
                        self.open_viewer(path);
                    }
                    _ if imgs.is_empty() => self.status_msg = "no images in this chat".into(),
                    _ => self.status_msg = format!("pick 1..{}", imgs.len()),
                }
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
            app.selected_chat_images(),
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
