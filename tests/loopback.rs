use std::sync::Arc;
use std::time::Duration;

use cweldrop::chain::Chains;
use cweldrop::event::AppEvent;
use cweldrop::identity::Identity;
use cweldrop::net::{self, NetCmd};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

fn fresh_identity() -> Arc<Identity> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("cweldrop-loopback-id-{ts}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    Arc::new(Identity::load_or_create_in(&dir).expect("identity"))
}

/// A chain store in a fresh temp dir.
fn fresh_chains(id: &Arc<Identity>) -> Arc<Chains> {
    let dir = std::env::temp_dir().join(format!("cweldrop-chains-{}", rand::random::<u64>()));
    Chains::open_in(Arc::clone(id), dir)
}

/// Spin up two NetSupervisors on 127.0.0.1, dial one from the other, exchange a text frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_supervisors_exchange_text() {
    let a_id = fresh_identity();
    let b_id = fresh_identity();
    let a_pubkey = a_id.pubkey_hex();
    let b_pubkey = b_id.pubkey_hex();

    let (a_evt_tx, mut a_evt_rx) = mpsc::channel::<AppEvent>(64);
    let (a_cmd_tx, a_cmd_rx) = mpsc::channel::<NetCmd>(64);
    let a_port = net::run_net(
        0,
        "alice".to_string(),
        Arc::clone(&a_id),
        fresh_chains(&a_id),
        a_cmd_rx,
        a_evt_tx,
    )
    .await
    .expect("start A");

    let (b_evt_tx, mut b_evt_rx) = mpsc::channel::<AppEvent>(64);
    let (b_cmd_tx, b_cmd_rx) = mpsc::channel::<NetCmd>(64);
    let _b_port = net::run_net(
        0,
        "bob".to_string(),
        Arc::clone(&b_id),
        fresh_chains(&b_id),
        b_cmd_rx,
        b_evt_tx,
    )
    .await
    .expect("start B");

    // B dials A.
    let a_addr = format!("127.0.0.1:{a_port}").parse().unwrap();
    let peer_id = "test-A".to_string();
    b_cmd_tx
        .send(NetCmd::Connect {
            id: peer_id.clone(),
            addr: a_addr,
        })
        .await
        .unwrap();

    // Wait for B to see PeerAuthenticated with A's pubkey.
    expect_event(
        &mut b_evt_rx,
        |e| matches!(e, AppEvent::PeerAuthenticated { pubkey, username, .. } if pubkey == &a_pubkey && username == "alice"),
        "B->Authenticated(alice)",
    )
    .await;

    // Wait for A to see an inbound PeerAuthenticated carrying bob's pubkey.
    let a_inbound_id = expect_event_extract(
        &mut a_evt_rx,
        |e| {
            if let AppEvent::PeerAuthenticated {
                id,
                pubkey,
                username,
            } = e
            {
                if pubkey == &b_pubkey && username == "bob" {
                    Some(id.clone())
                } else {
                    None
                }
            } else {
                None
            }
        },
        "A->Authenticated(bob)",
    )
    .await;

    // B sends a text message to A.
    b_cmd_tx
        .send(NetCmd::SendText {
            id: peer_id.clone(),
            body: "ping".to_string(),
        })
        .await
        .unwrap();

    // A should receive MessageReceived on its inbound peer id.
    expect_event(
        &mut a_evt_rx,
        |e| matches!(e, AppEvent::MessageReceived { id, body, .. } if *id == a_inbound_id && body == "ping"),
        "A->MessageReceived(ping)",
    )
    .await;

    // Cleanly shut down.
    let _ = b_cmd_tx.send(NetCmd::Disconnect { id: peer_id }).await;
    let _ = a_cmd_tx.send(NetCmd::Shutdown).await;
    let _ = b_cmd_tx.send(NetCmd::Shutdown).await;
}

/// Drive the handshake by hand: send a malformed Auth signature and confirm
/// the server emits a PeerError and tears the connection down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_signature_rejected() {
    let a_id = fresh_identity();
    let a_pubkey_hex = a_id.pubkey_hex();
    let (a_evt_tx, mut a_evt_rx) = mpsc::channel::<AppEvent>(64);
    let (a_cmd_tx, a_cmd_rx) = mpsc::channel::<NetCmd>(64);
    let a_port = net::run_net(
        0,
        "alice".to_string(),
        Arc::clone(&a_id),
        fresh_chains(&a_id),
        a_cmd_rx,
        a_evt_tx,
    )
    .await
    .expect("start A");

    let mut sock = TcpStream::connect(("127.0.0.1", a_port))
        .await
        .expect("dial");
    let (r, mut w) = sock.split();
    let mut r = BufReader::new(r);

    // Read A's hello.
    let mut line = String::new();
    r.read_line(&mut line).await.expect("read hello");
    assert!(line.contains("\"pubkey\""), "A's hello must carry pubkey");

    // Send our hello with a pubkey we don't actually own, plus a nonce.
    let fake_pubkey = "00".repeat(32);
    let nonce = "11".repeat(16);
    let our_hello = format!(
        "{{\"kind\":\"hello\",\"username\":\"mallory\",\"version\":\"0.0.0\",\"peer_id\":\"{fake_pubkey}\",\"pubkey\":\"{fake_pubkey}\",\"nonce\":\"{nonce}\"}}\n"
    );
    w.write_all(our_hello.as_bytes())
        .await
        .expect("write hello");

    // A will send its Auth, drain it.
    let mut auth_line = String::new();
    r.read_line(&mut auth_line).await.expect("read A auth");
    assert!(auth_line.contains("\"kind\":\"auth\""));

    // Send a deliberately wrong signature.
    let bad_sig = "ff".repeat(64);
    let our_auth = format!("{{\"kind\":\"auth\",\"signature\":\"{bad_sig}\"}}\n");
    w.write_all(our_auth.as_bytes())
        .await
        .expect("write bad auth");

    expect_event(
        &mut a_evt_rx,
        |e| matches!(e, AppEvent::PeerError { error, .. } if error.contains("auth")),
        "A->PeerError(auth)",
    )
    .await;

    let _ = a_cmd_tx.send(NetCmd::Shutdown).await;
    let _ = a_pubkey_hex; // silence unused
}

struct Pair {
    a_chains: Arc<Chains>,
    b_chains: Arc<Chains>,
    a_pubkey: String,
    b_pubkey: String,
    a_cmd: mpsc::Sender<NetCmd>,
    a_evt: mpsc::Receiver<AppEvent>,
    b_cmd: mpsc::Sender<NetCmd>,
    b_evt: mpsc::Receiver<AppEvent>,
    /// B's id for its connection to A.
    b_to_a: String,
    /// A's id for its inbound connection from B.
    a_to_b: String,
    a_addr: std::net::SocketAddr,
}

/// Two authenticated supervisors, B dialed A.
async fn connected_pair() -> Pair {
    let (a_id, b_id) = (fresh_identity(), fresh_identity());
    let (a_chains, b_chains) = (fresh_chains(&a_id), fresh_chains(&b_id));
    connected_pair_with(a_id, b_id, a_chains, b_chains).await
}

async fn connected_pair_with(
    a_id: Arc<Identity>,
    b_id: Arc<Identity>,
    a_chains: Arc<Chains>,
    b_chains: Arc<Chains>,
) -> Pair {
    let a_pubkey = a_id.pubkey_hex();
    let b_pubkey = b_id.pubkey_hex();
    let (a_evt_tx, mut a_evt) = mpsc::channel::<AppEvent>(256);
    let (a_cmd, a_cmd_rx) = mpsc::channel::<NetCmd>(64);
    let a_port = net::run_net(
        0,
        "alice".into(),
        a_id,
        Arc::clone(&a_chains),
        a_cmd_rx,
        a_evt_tx,
    )
    .await
    .expect("start A");
    let (b_evt_tx, mut b_evt) = mpsc::channel::<AppEvent>(256);
    let (b_cmd, b_cmd_rx) = mpsc::channel::<NetCmd>(64);
    net::run_net(
        0,
        "bob".into(),
        b_id,
        Arc::clone(&b_chains),
        b_cmd_rx,
        b_evt_tx,
    )
    .await
    .expect("start B");
    let b_to_a = "test-A".to_string();
    let a_addr: std::net::SocketAddr = format!("127.0.0.1:{a_port}").parse().unwrap();
    b_cmd
        .send(NetCmd::Connect {
            id: b_to_a.clone(),
            addr: a_addr,
        })
        .await
        .unwrap();
    expect_event(
        &mut b_evt,
        |e| matches!(e, AppEvent::PeerAuthenticated { .. }),
        "B authenticated",
    )
    .await;
    let a_to_b = expect_event_extract(
        &mut a_evt,
        |e| match e {
            AppEvent::PeerAuthenticated { id, pubkey, .. } if *pubkey == b_pubkey => {
                Some(id.clone())
            }
            _ => None,
        },
        "A authenticated",
    )
    .await;
    Pair {
        a_chains,
        b_chains,
        a_pubkey,
        b_pubkey,
        a_cmd,
        a_evt,
        b_cmd,
        b_evt,
        b_to_a,
        a_to_b,
        a_addr,
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cweldrop-{tag}-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// B offers a multi-chunk file, A accepts; a chat message sent mid-transfer
/// still arrives, and the saved copy is byte-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_transfer_accept() {
    let mut p = connected_pair().await;
    let src_dir = temp_dir("src");
    let dst_dir = temp_dir("dst");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let src = src_dir.join("photo.png");
    std::fs::write(&src, &data).unwrap();

    p.b_cmd
        .send(NetCmd::SendFile {
            id: p.b_to_a.clone(),
            file_id: "00ff00ff00ff00ff".into(),
            path: src.clone(),
            name: "../../photo.png".into(),
            size: data.len() as u64,
        })
        .await
        .unwrap();

    let (file_id, name, size) = expect_event_extract(
        &mut p.a_evt,
        |e| match e {
            AppEvent::FileOffered {
                file_id,
                name,
                size,
                ..
            } => Some((file_id.clone(), name.clone(), *size)),
            _ => None,
        },
        "A offered",
    )
    .await;
    assert_eq!(name, "photo.png", "offered name is sanitised");
    assert_eq!(size, data.len() as u64);

    p.a_cmd
        .send(NetCmd::AcceptFile {
            id: p.a_to_b.clone(),
            file_id: file_id.clone(),
            dir: dst_dir.clone(),
        })
        .await
        .unwrap();
    p.b_cmd
        .send(NetCmd::SendText {
            id: p.b_to_a.clone(),
            body: "during".into(),
        })
        .await
        .unwrap();

    let mut got_text = false;
    let mut saved = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while (saved.is_none() || !got_text) && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), p.a_evt.recv()).await {
            Ok(Some(AppEvent::MessageReceived { body, .. })) if body == "during" => got_text = true,
            Ok(Some(AppEvent::FileReceived {
                file_id: f, path, ..
            })) if f == file_id => saved = Some(path),
            Ok(Some(AppEvent::FileFailed { error, .. })) => panic!("transfer failed: {error}"),
            Ok(Some(_)) | Err(_) => {}
            Ok(None) => panic!("A channel closed"),
        }
    }
    assert!(got_text, "chat must flow during a transfer");
    let saved = saved.expect("file received");
    assert_eq!(saved, dst_dir.join("photo.png"));
    assert_eq!(std::fs::read(&saved).unwrap(), data);
    assert_eq!(
        std::fs::read_dir(&dst_dir).unwrap().count(),
        1,
        "no .part file left behind"
    );

    expect_event(
        &mut p.b_evt,
        |e| matches!(e, AppEvent::FileSent { file_id: f, .. } if *f == file_id),
        "B sent",
    )
    .await;

    let _ = p.a_cmd.send(NetCmd::Shutdown).await;
    let _ = p.b_cmd.send(NetCmd::Shutdown).await;
    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_transfer_reject() {
    let mut p = connected_pair().await;
    let src_dir = temp_dir("src");
    let src = src_dir.join("a.txt");
    std::fs::write(&src, b"hello").unwrap();
    p.b_cmd
        .send(NetCmd::SendFile {
            id: p.b_to_a.clone(),
            file_id: "abcd".into(),
            path: src,
            name: "a.txt".into(),
            size: 5,
        })
        .await
        .unwrap();
    expect_event(
        &mut p.a_evt,
        |e| matches!(e, AppEvent::FileOffered { .. }),
        "A offered",
    )
    .await;
    p.a_cmd
        .send(NetCmd::RejectFile {
            id: p.a_to_b.clone(),
            file_id: "abcd".into(),
        })
        .await
        .unwrap();
    expect_event(
        &mut p.b_evt,
        |e| matches!(e, AppEvent::FileRejected { file_id, .. } if file_id == "abcd"),
        "B rejected",
    )
    .await;
    let _ = p.a_cmd.send(NetCmd::Shutdown).await;
    let _ = p.b_cmd.send(NetCmd::Shutdown).await;
    let _ = std::fs::remove_dir_all(&src_dir);
}

/// Offer claims more bytes than the file holds: the sender notices the short
/// read and cancels, the receiver drops its partial file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn short_file_aborted() {
    let mut p = connected_pair().await;
    let src_dir = temp_dir("src");
    let dst_dir = temp_dir("dst");
    let src = src_dir.join("grow.bin");
    std::fs::write(&src, vec![1u8; 100_000]).unwrap();
    p.b_cmd
        .send(NetCmd::SendFile {
            id: p.b_to_a.clone(),
            file_id: "beef".into(),
            path: src,
            name: "grow.bin".into(),
            size: 200_000,
        })
        .await
        .unwrap();
    expect_event(
        &mut p.a_evt,
        |e| matches!(e, AppEvent::FileOffered { .. }),
        "A offered",
    )
    .await;
    p.a_cmd
        .send(NetCmd::AcceptFile {
            id: p.a_to_b.clone(),
            file_id: "beef".into(),
            dir: dst_dir.clone(),
        })
        .await
        .unwrap();
    expect_event(
        &mut p.b_evt,
        |e| matches!(e, AppEvent::FileFailed { file_id, .. } if file_id == "beef"),
        "B failed (short file)",
    )
    .await;
    expect_event(
        &mut p.a_evt,
        |e| matches!(e, AppEvent::FileFailed { file_id, .. } if file_id == "beef"),
        "A failed (cancelled)",
    )
    .await;
    assert_eq!(
        std::fs::read_dir(&dst_dir).unwrap().count(),
        0,
        "partial file removed"
    );
    let _ = p.a_cmd.send(NetCmd::Shutdown).await;
    let _ = p.b_cmd.send(NetCmd::Shutdown).await;
    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

fn chain_bodies(chains: &Chains, peer: &str) -> Vec<String> {
    chains
        .with(peer, |c| c.entries().into_iter().map(|e| e.body).collect())
        .unwrap()
}

/// A live chain message is verified and stored on the other side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chain_message_delivered() {
    let mut p = connected_pair().await;
    expect_event(
        &mut p.b_evt,
        |e| matches!(e, AppEvent::PeerChainReady { .. }),
        "B chain ready",
    )
    .await;
    let block = p.b_chains.compose(&p.a_pubkey, "signed hi").unwrap();
    let hash = block.hash.clone();
    p.b_cmd
        .send(NetCmd::SendBlock {
            id: p.b_to_a.clone(),
            block,
        })
        .await
        .unwrap();
    expect_event(
        &mut p.b_evt,
        |e| matches!(e, AppEvent::BlockDelivered { hash: h, .. } if *h == hash),
        "B delivered",
    )
    .await;
    expect_event(
        &mut p.a_evt,
        |e| matches!(e, AppEvent::ChainChanged { from_peer, .. } if from_peer == &["signed hi"]),
        "A got block",
    )
    .await;
    assert_eq!(chain_bodies(&p.a_chains, &p.b_pubkey), vec!["signed hi"]);
    let _ = p.a_cmd.send(NetCmd::Shutdown).await;
    let _ = p.b_cmd.send(NetCmd::Shutdown).await;
}

/// Both sides write while apart; connecting syncs them to the same
/// conversation in the same order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_messages_sync_on_connect() {
    let (a_id, b_id) = (fresh_identity(), fresh_identity());
    let (a_chains, b_chains) = (fresh_chains(&a_id), fresh_chains(&b_id));
    let (a_pk, b_pk) = (a_id.pubkey_hex(), b_id.pubkey_hex());
    a_chains.compose(&b_pk, "a while apart 1").unwrap();
    b_chains.compose(&a_pk, "b while apart").unwrap();
    a_chains.compose(&b_pk, "a while apart 2").unwrap();

    let mut p = connected_pair_with(a_id, b_id, a_chains, b_chains).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let a = chain_bodies(&p.a_chains, &p.b_pubkey);
        let b = chain_bodies(&p.b_chains, &p.a_pubkey);
        if a.len() == 3 && a == b {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "not synced: {a:?} vs {b:?}"
        );
        // Drain events so the peer tasks never block on a full channel.
        while p.a_evt.try_recv().is_ok() {}
        while p.b_evt.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let heads_a = p.a_chains.with(&p.b_pubkey, |c| c.heads()).unwrap();
    let heads_b = p.b_chains.with(&p.a_pubkey, |c| c.heads()).unwrap();
    assert_eq!(heads_a, heads_b);
    assert_eq!(
        heads_a.len(),
        2,
        "two branches until the next message merges them"
    );
    let _ = p.a_cmd.send(NetCmd::Shutdown).await;
    let _ = p.b_cmd.send(NetCmd::Shutdown).await;
}

/// After the remote hangs up, dialing the same peer id works again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redial_after_remote_hangup() {
    let mut p = connected_pair().await;
    p.a_cmd
        .send(NetCmd::Disconnect {
            id: p.a_to_b.clone(),
        })
        .await
        .unwrap();
    expect_event(
        &mut p.b_evt,
        |e| matches!(e, AppEvent::PeerDisconnected { .. }),
        "B saw hangup",
    )
    .await;
    // B's handle for this id is now dead; a redial must not be ignored.
    p.b_cmd
        .send(NetCmd::Connect {
            id: p.b_to_a.clone(),
            addr: p.a_addr,
        })
        .await
        .unwrap();
    expect_event(
        &mut p.b_evt,
        |e| matches!(e, AppEvent::PeerAuthenticated { .. }),
        "B re-authenticated",
    )
    .await;
    let _ = p.a_cmd.send(NetCmd::Shutdown).await;
    let _ = p.b_cmd.send(NetCmd::Shutdown).await;
}

async fn expect_event<F>(rx: &mut mpsc::Receiver<AppEvent>, pred: F, label: &str)
where
    F: Fn(&AppEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(ev)) => {
                if pred(&ev) {
                    return;
                }
            }
            Ok(None) => panic!("{label}: channel closed"),
            Err(_) => continue,
        }
    }
    panic!("{label}: timed out");
}

async fn expect_event_extract<F, T>(rx: &mut mpsc::Receiver<AppEvent>, f: F, label: &str) -> T
where
    F: Fn(&AppEvent) -> Option<T>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(ev)) => {
                if let Some(v) = f(&ev) {
                    return v;
                }
            }
            Ok(None) => panic!("{label}: channel closed"),
            Err(_) => continue,
        }
    }
    panic!("{label}: timed out");
}
