//! LAN discovery: advertise this server as a DNS-SD service over mDNS.

use std::net::SocketAddr;

use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};
use remcam_proto::{MDNS_SERVICE_TYPE, PROTOCOL_VERSION};
use tracing::info;

/// Start advertising. Returns `None` when listening on loopback, where there
/// is nobody to advertise to. The daemon stops advertising when dropped.
pub fn advertise(
    instance: &str,
    listen: SocketAddr,
    fingerprint: &str,
) -> anyhow::Result<Option<ServiceDaemon>> {
    if listen.ip().is_loopback() {
        return Ok(None);
    }
    let daemon = ServiceDaemon::new()?;
    // Advertising 127.0.0.1 would lead remote clients to connect to themselves.
    daemon.disable_interface(vec![IfKind::LoopbackV4, IfKind::LoopbackV6])?;
    let version = PROTOCOL_VERSION.to_string();
    let properties = [("v", version.as_str()), ("fp", fingerprint)];
    let host = format!("{instance}.local.");
    let service = if listen.ip().is_unspecified() {
        ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            instance,
            &host,
            "",
            listen.port(),
            &properties[..],
        )?
        .enable_addr_auto()
    } else {
        ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            instance,
            &host,
            listen.ip(),
            listen.port(),
            &properties[..],
        )?
    };
    daemon.register(service)?;
    info!("advertising as \"{instance}\" ({MDNS_SERVICE_TYPE})");
    Ok(Some(daemon))
}
