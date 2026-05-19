use std::net::SocketAddr;

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;
use tracing::{debug, info};


use crate::event::AppEvent;
use crate::net::PeerId;

pub const SERVICE_TYPE: &str = "_cweldrop._tcp.local.";

pub struct Discovery {
    daemon: ServiceDaemon,
    instance_name: String,
}

impl Discovery {
    pub fn start(username: &str, port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("mdns daemon")?;
        let host = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "host".to_string());
        // Instance name must be unique on the network; include port.
        let instance_name = format!("{username}@{host}-{port}");
        let host_local = format!("{host}.local.");

        let props: [(&str, &str); 2] = [
            ("username", username),
            ("version", env!("CARGO_PKG_VERSION")),
        ];
        // Empty addrs slice + enable_addr_auto lets mdns-sd pick all interfaces.
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name,
            &host_local,
            "",
            port,
            &props[..],
        )
        .context("ServiceInfo")?
        .enable_addr_auto();

        daemon.register(info).context("register mdns service")?;
        info!(%instance_name, port, "mdns registered");

        Ok(Self {
            daemon,
            instance_name,
        })
    }

    pub fn spawn_browser(&self, evt_tx: mpsc::Sender<AppEvent>) -> Result<()> {
        let recv = self
            .daemon
            .browse(SERVICE_TYPE)
            .context("start mdns browse")?;
        let own_instance = self.instance_name.clone();
        tokio::task::spawn_blocking(move || {
            while let Ok(event) = recv.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let fullname = info.get_fullname().to_string();
                        if fullname.starts_with(&format!("{own_instance}.")) {
                            debug!(%fullname, "ignoring self");
                            continue;
                        }
                        let port = info.get_port();
                        let Some(ip) = info.get_addresses().iter().next().copied() else {
                            continue;
                        };
                        let sock = SocketAddr::new(ip, port);
                        let username = info
                            .get_property_val_str("username")
                            .unwrap_or(info.get_fullname())
                            .to_string();
                        let id = peer_id_from_instance(info.get_fullname());
                        let _ = evt_tx.blocking_send(AppEvent::PeerDiscovered {
                            id,
                            username,
                            addr: sock,
                        });
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        let id = peer_id_from_instance(&fullname);
                        let _ = evt_tx.blocking_send(AppEvent::PeerLost { id });
                    }
                    _ => {}
                }
            }
        });
        Ok(())
    }

    pub fn shutdown(self) {
        let _ = self.daemon.shutdown();
    }
}

pub fn peer_id_from_instance(fullname: &str) -> PeerId {
    format!("mdns:{fullname}")
}
