use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::SystemTime;

use chrono::{DateTime, Utc};

use crate::event::AppEvent;
use crate::history_store;
use crate::net::{NetCmd, PeerId};

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
}

impl Peer {
    /// Key under which chat lines are stored in `App.history` for this peer.
    /// Verified peers use their pubkey so transcripts survive across nicks and
    /// connection ids; unverified peers fall back to the connection id.
    pub fn history_key(&self) -> &str {
        self.pubkey.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    pub from: String,
    pub body: String,
    pub at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Peers,
    Input,
    Command,
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
    pub command: String,
    pub status_msg: String,
    pub should_quit: bool,
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
            command: String::new(),
            status_msg: String::new(),
            should_quit: false,
        }
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
                self.history.entry(key).or_default().push(ChatLine {
                    from,
                    body,
                    at: SystemTime::now(),
                });
            }
        }
    }

    /// Append a sent message to the in-memory history. Disk persistence is
    /// the net task's responsibility (peer.rs writes after the wire send).
    pub fn push_self_message(&mut self, peer_id: &str, body: String) {
        let key = self.history_key_for_id(peer_id);
        self.history.entry(key).or_default().push(ChatLine {
            from: self.self_username.clone(),
            body,
            at: SystemTime::now(),
        });
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
}
