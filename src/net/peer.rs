use std::net::SocketAddr;
use std::sync::Arc;

use chrono::Utc;
use rand::RngCore;
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::protocol::{read_frame, write_frame, Message};
use super::PeerId;
use crate::event::AppEvent;
use crate::history_store::{self, Direction, StoredLine};
use crate::identity::{self, Identity};

#[allow(clippy::too_many_arguments)]
pub async fn run_peer(
    id: PeerId,
    stream: TcpStream,
    addr: SocketAddr,
    self_username: String,
    identity: Arc<Identity>,
    mut out_rx: mpsc::Receiver<Message>,
    evt_tx: mpsc::Sender<AppEvent>,
    dialed: bool,
) {
    let _ = stream.set_nodelay(true);
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    let _ = evt_tx
        .send(AppEvent::PeerConnecting {
            id: id.clone(),
            addr,
            dialed,
        })
        .await;

    // ---- Handshake ----

    let mut local_nonce = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut local_nonce);
    let local_nonce_hex = hex::encode(local_nonce);
    let my_pubkey = identity.pubkey_bytes();
    let my_pubkey_hex = identity.pubkey_hex();

    let hello = Message::hello_signed(&self_username, &my_pubkey_hex, &local_nonce_hex);
    if let Err(e) = write_frame(&mut w, &hello).await {
        warn!(error=%e, %addr, "hello write failed");
        let _ = evt_tx
            .send(AppEvent::PeerError {
                id: id.clone(),
                error: format!("write hello: {e}"),
            })
            .await;
        return;
    }

    let (remote_username, remote_pubkey_hex, remote_pubkey_bytes, remote_nonce) =
        match read_frame(&mut r).await {
            Ok(Some(Message::Hello {
                username,
                version,
                pubkey,
                nonce,
                peer_id: _,
            })) => {
                debug!(%username, %version, %addr, "peer hello");
                let pk_bytes = pubkey.as_deref().and_then(decode_pubkey);
                let nonce_bytes = nonce.as_deref().and_then(|s| hex::decode(s).ok());
                (username, pubkey, pk_bytes, nonce_bytes)
            }
            Ok(Some(other)) => {
                warn!(?other, %addr, "expected hello, got other frame");
                let _ = evt_tx
                    .send(AppEvent::PeerError {
                        id: id.clone(),
                        error: "protocol: expected hello".into(),
                    })
                    .await;
                return;
            }
            Ok(None) => {
                info!(%addr, "peer closed before hello");
                let _ = evt_tx
                    .send(AppEvent::PeerDisconnected { id: id.clone() })
                    .await;
                return;
            }
            Err(e) => {
                warn!(error=%e, %addr, "read hello error");
                let _ = evt_tx
                    .send(AppEvent::PeerError {
                        id: id.clone(),
                        error: format!("read hello: {e}"),
                    })
                    .await;
                return;
            }
        };

    let authenticated_pubkey: Option<String> = match (remote_pubkey_bytes, remote_nonce) {
        (Some(rpk), Some(rn)) => {
            // Sign: remote_nonce || remote_pubkey || my_pubkey
            let mut to_sign = Vec::with_capacity(rn.len() + 32 + 32);
            to_sign.extend_from_slice(&rn);
            to_sign.extend_from_slice(&rpk);
            to_sign.extend_from_slice(&my_pubkey);
            let sig = identity.sign(&to_sign);
            let auth_msg = Message::Auth {
                signature: hex::encode(sig.to_bytes()),
            };
            if let Err(e) = write_frame(&mut w, &auth_msg).await {
                warn!(error=%e, %addr, "auth write failed");
                let _ = evt_tx
                    .send(AppEvent::PeerError {
                        id: id.clone(),
                        error: format!("write auth: {e}"),
                    })
                    .await;
                return;
            }

            match read_frame(&mut r).await {
                Ok(Some(Message::Auth { signature })) => {
                    let sig_bytes = match decode_sig(&signature) {
                        Some(b) => b,
                        None => {
                            warn!(%addr, "auth signature decode failed");
                            let _ = evt_tx
                                .send(AppEvent::PeerError {
                                    id: id.clone(),
                                    error: "auth: bad signature encoding".into(),
                                })
                                .await;
                            return;
                        }
                    };
                    // Verify: local_nonce || my_pubkey || remote_pubkey
                    let mut to_verify = Vec::with_capacity(local_nonce.len() + 32 + 32);
                    to_verify.extend_from_slice(&local_nonce);
                    to_verify.extend_from_slice(&my_pubkey);
                    to_verify.extend_from_slice(&rpk);
                    if !identity::verify(&rpk, &to_verify, &sig_bytes) {
                        warn!(%addr, "auth signature invalid");
                        let _ = evt_tx
                            .send(AppEvent::PeerError {
                                id: id.clone(),
                                error: "auth: signature did not verify".into(),
                            })
                            .await;
                        return;
                    }
                    remote_pubkey_hex.clone()
                }
                Ok(Some(other)) => {
                    warn!(?other, %addr, "expected auth, got other frame");
                    let _ = evt_tx
                        .send(AppEvent::PeerError {
                            id: id.clone(),
                            error: "protocol: expected auth".into(),
                        })
                        .await;
                    return;
                }
                Ok(None) => {
                    info!(%addr, "peer closed before auth");
                    let _ = evt_tx
                        .send(AppEvent::PeerDisconnected { id: id.clone() })
                        .await;
                    return;
                }
                Err(e) => {
                    warn!(error=%e, %addr, "read auth error");
                    let _ = evt_tx
                        .send(AppEvent::PeerError {
                            id: id.clone(),
                            error: format!("read auth: {e}"),
                        })
                        .await;
                    return;
                }
            }
        }
        _ => None, // legacy peer: no pubkey / nonce
    };

    let _ = evt_tx
        .send(AppEvent::PeerConnected {
            id: id.clone(),
            username: remote_username.clone(),
            resolved_id: authenticated_pubkey.clone(),
        })
        .await;

    if let Some(pk) = authenticated_pubkey.as_ref() {
        let _ = evt_tx
            .send(AppEvent::PeerAuthenticated {
                id: id.clone(),
                pubkey: pk.clone(),
                username: remote_username.clone(),
            })
            .await;
    }

    // ---- Main loop ----

    loop {
        tokio::select! {
            res = read_frame(&mut r) => {
                match res {
                    Ok(Some(msg)) => match msg {
                        Message::Hello { .. } | Message::Auth { .. } => {
                            warn!(%addr, "unexpected handshake frame after handshake");
                        }
                        Message::Text { body } => {
                            let from = remote_username.clone();
                            if let Some(pk) = authenticated_pubkey.as_ref() {
                                if let Err(e) = history_store::append(pk, &StoredLine {
                                    dir: Direction::Recv,
                                    from: from.clone(),
                                    body: body.clone(),
                                    at: Utc::now(),
                                }) {
                                    warn!(error=%e, "history append (recv) failed");
                                }
                            }
                            let _ = evt_tx.send(AppEvent::MessageReceived {
                                id: id.clone(),
                                from,
                                body,
                            }).await;
                        }
                        Message::Typing { active } => {
                            let _ = evt_tx.send(AppEvent::PeerTyping {
                                id: id.clone(),
                                active,
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
                        let text_body = if let Message::Text { body } = &msg {
                            Some(body.clone())
                        } else {
                            None
                        };
                        if let Err(e) = write_frame(&mut w, &msg).await {
                            warn!(error=%e, %addr, "write error");
                            break;
                        }
                        if let (Some(body), Some(pk)) = (text_body, authenticated_pubkey.as_ref()) {
                            if let Err(e) = history_store::append(pk, &StoredLine {
                                dir: Direction::Sent,
                                from: self_username.clone(),
                                body,
                                at: Utc::now(),
                            }) {
                                warn!(error=%e, "history append (sent) failed");
                            }
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

fn decode_pubkey(s: &str) -> Option<[u8; 32]> {
    let v = hex::decode(s).ok()?;
    if v.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Some(out)
}

fn decode_sig(s: &str) -> Option<[u8; 64]> {
    let v = hex::decode(s).ok()?;
    if v.len() != 64 {
        return None;
    }
    let mut out = [0u8; 64];
    out.copy_from_slice(&v);
    Some(out)
}
