use std::sync::Arc;
use std::time::Duration;

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
        a_cmd_rx,
        a_evt_tx,
    )
    .await
    .expect("start A");

    let (b_evt_tx, mut b_evt_rx) = mpsc::channel::<AppEvent>(64);
    let (b_cmd_tx, b_cmd_rx) = mpsc::channel::<NetCmd>(64);
    let _b_port = net::run_net(0, "bob".to_string(), Arc::clone(&b_id), b_cmd_rx, b_evt_tx)
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
