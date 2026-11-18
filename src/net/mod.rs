pub mod client;
pub mod peer;
pub mod protocol;
pub mod server;
pub mod transfer;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::chain::Chains;
use crate::event::AppEvent;
use crate::identity::Identity;
use protocol::Message;

pub type PeerId = String;

#[derive(Debug)]
pub enum NetCmd {
    Connect {
        id: PeerId,
        addr: SocketAddr,
    },
    SendText {
        id: PeerId,
        body: String,
    },
    SendTyping {
        id: PeerId,
        active: bool,
    },
    /// Deliver a chain block (already stored locally).
    SendBlock {
        id: PeerId,
        block: crate::chain::Block,
    },
    /// Offer a local file to the peer.
    SendFile {
        id: PeerId,
        file_id: String,
        path: PathBuf,
        name: String,
        size: u64,
    },
    /// Accept a pending offer, saving into `dir`.
    AcceptFile {
        id: PeerId,
        file_id: String,
        dir: PathBuf,
    },
    RejectFile {
        id: PeerId,
        file_id: String,
    },
    Disconnect {
        id: PeerId,
    },
    Shutdown,
}

/// Instructions from the supervisor to one peer task.
#[derive(Debug)]
pub enum PeerCmd {
    Send(Message),
    SendFile {
        file_id: String,
        path: PathBuf,
        name: String,
        size: u64,
    },
    AcceptFile {
        file_id: String,
        dir: PathBuf,
    },
    RejectFile {
        file_id: String,
    },
}

struct PeerHandle {
    tx: mpsc::Sender<PeerCmd>,
}

pub struct NetSupervisor {
    username: String,
    identity: Arc<Identity>,
    chains: Arc<Chains>,
    peers: HashMap<PeerId, PeerHandle>,
    cmd_rx: mpsc::Receiver<NetCmd>,
    evt_tx: mpsc::Sender<AppEvent>,
}

impl NetSupervisor {
    pub fn new(
        username: String,
        identity: Arc<Identity>,
        chains: Arc<Chains>,
        cmd_rx: mpsc::Receiver<NetCmd>,
        evt_tx: mpsc::Sender<AppEvent>,
    ) -> Self {
        Self {
            username,
            identity,
            chains,
            peers: HashMap::new(),
            cmd_rx,
            evt_tx,
        }
    }

    pub async fn run(mut self, mut incoming: mpsc::Receiver<(TcpStream, SocketAddr)>) {
        loop {
            tokio::select! {
                Some(cmd) = self.cmd_rx.recv() => {
                    if matches!(cmd, NetCmd::Shutdown) {
                        info!("supervisor shutdown");
                        break;
                    }
                    self.handle_cmd(cmd).await;
                }
                Some((stream, addr)) = incoming.recv() => {
                    self.spawn_inbound(stream, addr).await;
                }
                else => break,
            }
        }
    }

    async fn handle_cmd(&mut self, cmd: NetCmd) {
        // Drop handles of finished peer tasks first, so a dead connection
        // never blocks redialing the same peer id.
        self.peers.retain(|_, h| !h.tx.is_closed());
        match cmd {
            NetCmd::Connect { id, addr } => {
                if self.peers.contains_key(&id) {
                    return;
                }
                let username = self.username.clone();
                let identity = Arc::clone(&self.identity);
                let chains = Arc::clone(&self.chains);
                let evt_tx = self.evt_tx.clone();
                let (out_tx, out_rx) = mpsc::channel::<PeerCmd>(64);
                self.peers.insert(id.clone(), PeerHandle { tx: out_tx });
                tokio::spawn(async move {
                    match TcpStream::connect(addr).await {
                        Ok(stream) => {
                            peer::run_peer(
                                id, stream, addr, username, identity, chains, out_rx, evt_tx, true,
                            )
                            .await;
                        }
                        Err(e) => {
                            warn!(error=%e, %addr, "dial failed");
                            let _ = evt_tx
                                .send(AppEvent::PeerError {
                                    id,
                                    error: format!("dial failed: {e}"),
                                })
                                .await;
                        }
                    }
                });
            }
            NetCmd::SendText { id, body } => {
                self.to_peer(&id, PeerCmd::Send(Message::text(body))).await;
            }
            NetCmd::SendBlock { id, block } => {
                // Not connected: the block stays queued in the chain and goes
                // out with the next sync.
                self.to_peer(&id, PeerCmd::Send(Message::ChainBlock { block }))
                    .await;
            }
            NetCmd::SendTyping { id, active } => {
                self.to_peer(&id, PeerCmd::Send(Message::Typing { active }))
                    .await;
            }
            NetCmd::SendFile {
                id,
                file_id,
                path,
                name,
                size,
            } => {
                let cmd = PeerCmd::SendFile {
                    file_id: file_id.clone(),
                    path,
                    name,
                    size,
                };
                if !self.to_peer(&id, cmd).await {
                    let _ = self
                        .evt_tx
                        .send(AppEvent::FileFailed {
                            id,
                            file_id,
                            error: "peer not connected".into(),
                        })
                        .await;
                }
            }
            NetCmd::AcceptFile { id, file_id, dir } => {
                self.to_peer(&id, PeerCmd::AcceptFile { file_id, dir })
                    .await;
            }
            NetCmd::RejectFile { id, file_id } => {
                self.to_peer(&id, PeerCmd::RejectFile { file_id }).await;
            }
            NetCmd::Disconnect { id } => {
                if let Some(h) = self.peers.remove(&id) {
                    let _ = h.tx.send(PeerCmd::Send(Message::Bye)).await;
                }
            }
            NetCmd::Shutdown => {}
        }
        // Cull peers whose writer is closed.
        self.peers.retain(|_, h| !h.tx.is_closed());
    }

    /// Forward to a peer task; false if there's no live task for `id`.
    async fn to_peer(&self, id: &str, cmd: PeerCmd) -> bool {
        match self.peers.get(id) {
            Some(h) => h.tx.send(cmd).await.is_ok(),
            None => false,
        }
    }

    async fn spawn_inbound(&mut self, stream: TcpStream, addr: SocketAddr) {
        let id = format!("inbound:{addr}");
        if self.peers.contains_key(&id) {
            return;
        }
        let (out_tx, out_rx) = mpsc::channel::<PeerCmd>(64);
        self.peers.insert(id.clone(), PeerHandle { tx: out_tx });
        let evt_tx = self.evt_tx.clone();
        let username = self.username.clone();
        let identity = Arc::clone(&self.identity);
        let chains = Arc::clone(&self.chains);
        tokio::spawn(async move {
            peer::run_peer(
                id, stream, addr, username, identity, chains, out_rx, evt_tx, false,
            )
            .await;
        });
    }
}

/// Glue: own a NetSupervisor + server, run them together.
pub async fn run_net(
    bind_port: u16,
    username: String,
    identity: Arc<Identity>,
    chains: Arc<Chains>,
    cmd_rx: mpsc::Receiver<NetCmd>,
    evt_tx: mpsc::Sender<AppEvent>,
) -> Result<u16> {
    let (incoming_tx, incoming_rx) = mpsc::channel::<(TcpStream, SocketAddr)>(32);
    let (listener, actual_port) = server::bind(bind_port).await?;
    tokio::spawn(server::accept_loop(listener, incoming_tx));
    let sup = NetSupervisor::new(username, identity, chains, cmd_rx, evt_tx);
    tokio::spawn(sup.run(incoming_rx));
    Ok(actual_port)
}
