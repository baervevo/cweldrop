use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::SystemTime;

use crate::event::AppEvent;
use crate::net::{NetCmd, PeerId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStatus {
    Available,    // discovered via mDNS, not yet connected
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
    pub peers: Vec<Peer>,
    pub history: HashMap<PeerId, Vec<ChatLine>>,
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
                    });
                }
            }
            AppEvent::PeerConnected { id, username, resolved_id } => {
                // If the remote advertised its mdns peer_id and we already have a
                // discovery-only entry under that id, merge: keep the connected
                // (current) entry, drop the duplicate.
                if let Some(other) = resolved_id.as_ref() {
                    if other != &id {
                        if let Some(cur_idx) = self.peer_index(&id) {
                            if let Some(dup_idx) = self.peer_index(other) {
                                // Preserve any history accumulated under the duplicate.
                                if let Some(hist) = self.history.remove(other) {
                                    self.history.entry(id.clone()).or_default().extend(hist);
                                }
                                // Inherit a better addr if we have none.
                                if self.peers[cur_idx].addr.is_none() {
                                    self.peers[cur_idx].addr = self.peers[dup_idx].addr;
                                }
                                self.peers.remove(dup_idx);
                                if self.selected >= self.peers.len() && !self.peers.is_empty() {
                                    self.selected = self.peers.len() - 1;
                                }
                            }
                        }
                    }
                }
                if let Some(idx) = self.peer_index(&id) {
                    self.peers[idx].status = PeerStatus::Online;
                    if !username.is_empty() {
                        self.peers[idx].username = username;
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
                self.history.entry(id).or_default().push(ChatLine {
                    from,
                    body,
                    at: SystemTime::now(),
                });
            }
        }
    }

    pub fn push_self_message(&mut self, peer_id: &str, body: String) {
        self.history.entry(peer_id.to_string()).or_default().push(ChatLine {
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
