use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};

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
        // Advertise only routable addresses. Link-local IPv6 (fe80::/10) requires a
        // scope_id that mdns-sd does not propagate to clients, so connect() fails
        // with EINVAL. Filter it out at the source.
        let addrs = routable_local_addrs();
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name,
            &host_local,
            &addrs[..],
            port,
            &props[..],
        )
        .context("ServiceInfo")?;

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
                        let Some(ip) = pick_addr(info.get_addresses().iter().copied()) else {
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

fn is_ipv6_link_local(ip: &std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

fn routable_local_addrs() -> Vec<IpAddr> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut seen: HashSet<IpAddr> = HashSet::new();
    let mut out = Vec::new();
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        let ip = iface.ip();
        if let IpAddr::V6(v6) = ip {
            if is_ipv6_link_local(&v6) {
                continue;
            }
        }
        if seen.insert(ip) {
            out.push(ip);
        }
    }
    out
}

// Prefer routable addresses: IPv4, then global IPv6. Link-local IPv6 (fe80::/10)
// requires a scope_id that mdns-sd does not surface, and connect() fails with EINVAL.
fn pick_addr<I: IntoIterator<Item = IpAddr>>(addrs: I) -> Option<IpAddr> {
    let mut v4 = None;
    let mut v6_global = None;
    let mut v6_link = None;
    for ip in addrs {
        match ip {
            IpAddr::V4(_) => {
                if v4.is_none() {
                    v4 = Some(ip);
                }
            }
            IpAddr::V6(a) => {
                let seg = a.segments()[0];
                let is_link_local = (seg & 0xffc0) == 0xfe80;
                if is_link_local {
                    if v6_link.is_none() {
                        v6_link = Some(ip);
                    }
                } else if v6_global.is_none() {
                    v6_global = Some(ip);
                }
            }
        }
    }
    v4.or(v6_global).or(v6_link)
}
