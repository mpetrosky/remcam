//! LAN discovery of remcam servers via mDNS / DNS-SD.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use remcam_proto::MDNS_SERVICE_TYPE;

#[derive(Debug, Clone)]
pub struct Server {
    /// Instance name, normally the server's hostname.
    pub name: String,
    pub addrs: Vec<IpAddr>,
    pub port: u16,
    /// Advertised certificate fingerprint. mDNS is unauthenticated, so this
    /// is a hint to show the user, never something to trust automatically.
    pub fingerprint: Option<String>,
    pub version: Option<u32>,
}

impl Server {
    /// Preferred address to connect to: IPv4, then routable IPv6, then
    /// link-local IPv6. Loopback comes last; it is only right when the
    /// server is on this machine, and then the other addresses work too.
    pub fn addr(&self) -> Option<SocketAddr> {
        let rank = |ip: &IpAddr| match ip {
            ip if ip.is_loopback() => 3,
            IpAddr::V4(_) => 0,
            IpAddr::V6(v6) if !v6.is_unicast_link_local() => 1,
            IpAddr::V6(_) => 2,
        };
        self.addrs
            .iter()
            .min_by_key(|ip| rank(ip))
            .map(|ip| SocketAddr::new(*ip, self.port))
    }
}

/// Listen for server advertisements for `wait`, then return what was found.
pub async fn browse(wait: Duration) -> anyhow::Result<Vec<Server>> {
    tokio::task::spawn_blocking(move || {
        let daemon = ServiceDaemon::new()?;
        let events = daemon.browse(MDNS_SERVICE_TYPE)?;
        let deadline = Instant::now() + wait;
        let mut found = HashMap::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(event) = events.recv_timeout(left) else {
                break;
            };
            if let ServiceEvent::ServiceResolved(info) = event {
                let name = info
                    .fullname
                    .strip_suffix(&format!(".{MDNS_SERVICE_TYPE}"))
                    .unwrap_or(&info.fullname)
                    .to_string();
                let mut addrs: Vec<IpAddr> =
                    info.addresses.iter().map(|a| a.to_ip_addr()).collect();
                addrs.sort();
                found.insert(
                    info.fullname.clone(),
                    Server {
                        name,
                        addrs,
                        port: info.port,
                        fingerprint: info.get_property_val_str("fp").map(str::to_string),
                        version: info.get_property_val_str("v").and_then(|v| v.parse().ok()),
                    },
                );
            }
        }
        let _ = daemon.shutdown();
        let mut servers: Vec<Server> = found.into_values().collect();
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(servers)
    })
    .await?
}
