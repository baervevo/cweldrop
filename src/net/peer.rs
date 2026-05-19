use std::net::SocketAddr;

use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::protocol::{read_frame, write_frame, Message};
use super::PeerId;
use crate::event::AppEvent;

#[allow(clippy::too_many_arguments)]
pub async fn run_peer(
    id: PeerId,
    stream: TcpStream,
    addr: SocketAddr,
    self_username: String,
    mut out_rx: mpsc::Receiver<Message>,
    evt_tx: mpsc::Sender<AppEvent>,
    dialed: bool,
) {
    let _ = stream.set_nodelay(true);
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    // Send hello first.
    if let Err(e) = write_frame(&mut w, &Message::hello(&self_username)).await {
        warn!(error=%e, %addr, "hello write failed");
        let _ = evt_tx
            .send(AppEvent::PeerError {
                id: id.clone(),
                error: format!("write hello: {e}"),
            })
            .await;
        return;
    }

    let mut remote_username: Option<String> = None;
    let _ = evt_tx
        .send(AppEvent::PeerConnecting {
            id: id.clone(),
            addr,
            dialed,
        })
        .await;

    loop {
        tokio::select! {
            res = read_frame(&mut r) => {
                match res {
                    Ok(Some(msg)) => match msg {
                        Message::Hello { username, version } => {
                            debug!(%username, %version, %addr, "peer hello");
                            remote_username = Some(username.clone());
                            let _ = evt_tx.send(AppEvent::PeerConnected {
                                id: id.clone(),
                                username,
                            }).await;
                        }
                        Message::Text { body } => {
                            let from = remote_username.clone().unwrap_or_else(|| id.clone());
                            let _ = evt_tx.send(AppEvent::MessageReceived {
                                id: id.clone(),
                                from,
                                body,
                            }).await;
                        }
                        Message::Bye => {
                            info!(%addr, "peer said bye");
                            break;
                        }
                    },
                    Ok(None) => {
                        info!(%addr, "peer closed");
                        break;
                    }
                    Err(e) => {
                        warn!(error=%e, %addr, "read error");
                        break;
                    }
                }
            }
            out = out_rx.recv() => {
                match out {
                    Some(msg) => {
                        let is_bye = matches!(msg, Message::Bye);
                        if let Err(e) = write_frame(&mut w, &msg).await {
                            warn!(error=%e, %addr, "write error");
                            break;
                        }
                        if is_bye { break; }
                    }
                    None => break,
                }
            }
        }
    }

    let _ = evt_tx
        .send(AppEvent::PeerDisconnected { id: id.clone() })
        .await;
}
