use std::time::Duration;

use cweldrop::event::AppEvent;
use cweldrop::net::{self, NetCmd};
use tokio::sync::mpsc;

/// Spin up two NetSupervisors on 127.0.0.1, dial one from the other, exchange a text frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_supervisors_exchange_text() {
    let (a_evt_tx, mut a_evt_rx) = mpsc::channel::<AppEvent>(64);
    let (a_cmd_tx, a_cmd_rx) = mpsc::channel::<NetCmd>(64);
    let a_port = net::run_net(0, "alice".to_string(), a_cmd_rx, a_evt_tx)
        .await
        .expect("start A");

    let (b_evt_tx, mut b_evt_rx) = mpsc::channel::<AppEvent>(64);
    let (b_cmd_tx, b_cmd_rx) = mpsc::channel::<NetCmd>(64);
    let _b_port = net::run_net(0, "bob".to_string(), b_cmd_rx, b_evt_tx)
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

    // Wait for B to see PeerConnected (i.e. A's hello arrived at B).
    expect_event(
        &mut b_evt_rx,
        |e| matches!(e, AppEvent::PeerConnected { username, .. } if username == "alice"),
        "B->Connected(alice)",
    )
    .await;

    // Wait for A to see an inbound PeerConnected with bob's hello.
    let a_inbound_id = expect_event_extract(
        &mut a_evt_rx,
        |e| {
            if let AppEvent::PeerConnected { id, username } = e {
                if username == "bob" { Some(id.clone()) } else { None }
            } else {
                None
            }
        },
        "A->Connected(bob)",
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
