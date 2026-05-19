pub mod client;
pub mod peer;
pub mod protocol;
pub mod server;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::event::AppEvent;
use crate::identity::Identity;
use protocol::Message;

pub type PeerId = String;

#[derive(Debug)]
pub enum NetCmd {
    Connect { id: PeerId, addr: SocketAddr },
    SendText { id: PeerId, body: String },
    Disconnect { id: PeerId },
    Shutdown,
}

struct PeerHandle {
    tx: mpsc::Sender<Message>,
}

pub struct NetSupervisor {
    username: String,
    identity: Arc<Identity>,
    peers: HashMap<PeerId, PeerHandle>,
    cmd_rx: mpsc::Receiver<NetCmd>,
    evt_tx: mpsc::Sender<AppEvent>,
}

impl NetSupervisor {
    pub fn new(
        username: String,
        identity: Arc<Identity>,
        cmd_rx: mpsc::Receiver<NetCmd>,
        evt_tx: mpsc::Sender<AppEvent>,
    ) -> Self {
        Self {
            username,
            identity,
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
        match cmd {
            NetCmd::Connect { id, addr } => {
                if self.peers.contains_key(&id) {
                    return;
                }
                let username = self.username.clone();
                let identity = Arc::clone(&self.identity);
                let evt_tx = self.evt_tx.clone();
                let (out_tx, out_rx) = mpsc::channel::<Message>(64);
                self.peers.insert(id.clone(), PeerHandle { tx: out_tx });
                tokio::spawn(async move {
                    match TcpStream::connect(addr).await {
                        Ok(stream) => {
                            peer::run_peer(
                                id, stream, addr, username, identity, out_rx, evt_tx, true,
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
                if let Some(h) = self.peers.get(&id) {
                    let _ = h.tx.send(Message::text(body)).await;
                }
            }
            NetCmd::Disconnect { id } => {
                if let Some(h) = self.peers.remove(&id) {
                    let _ = h.tx.send(Message::Bye).await;
                }
            }
            NetCmd::Shutdown => {}
        }
        // Cull peers whose writer is closed.
        self.peers.retain(|_, h| !h.tx.is_closed());
    }

    async fn spawn_inbound(&mut self, stream: TcpStream, addr: SocketAddr) {
        let id = format!("inbound:{addr}");
        if self.peers.contains_key(&id) {
            return;
        }
        let (out_tx, out_rx) = mpsc::channel::<Message>(64);
        self.peers.insert(id.clone(), PeerHandle { tx: out_tx });
        let evt_tx = self.evt_tx.clone();
        let username = self.username.clone();
        let identity = Arc::clone(&self.identity);
        tokio::spawn(async move {
            peer::run_peer(id, stream, addr, username, identity, out_rx, evt_tx, false).await;
        });
    }
}

/// Glue: own a NetSupervisor + server, run them together.
pub async fn run_net(
    bind_port: u16,
    username: String,
    identity: Arc<Identity>,
    cmd_rx: mpsc::Receiver<NetCmd>,
    evt_tx: mpsc::Sender<AppEvent>,
) -> Result<u16> {
    let (incoming_tx, incoming_rx) = mpsc::channel::<(TcpStream, SocketAddr)>(32);
    let (listener, actual_port) = server::bind(bind_port).await?;
    tokio::spawn(server::accept_loop(listener, incoming_tx));
    let sup = NetSupervisor::new(username, identity, cmd_rx, evt_tx);
    tokio::spawn(sup.run(incoming_rx));
    Ok(actual_port)
}
