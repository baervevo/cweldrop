use std::net::SocketAddr;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{info, warn};

pub async fn bind(port: u16) -> Result<(TcpListener, u16)> {
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let listener = TcpListener::bind(addr).await?;
    let actual = listener.local_addr()?.port();
    info!(%addr, actual_port = actual, "listening");
    Ok((listener, actual))
}

pub async fn accept_loop(listener: TcpListener, out: mpsc::Sender<(TcpStream, SocketAddr)>) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                info!(%addr, "inbound connection");
                if out.send((stream, addr)).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                warn!(error=%e, "accept failed");
            }
        }
    }
}
