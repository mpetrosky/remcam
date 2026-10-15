//! remcam-client: receives a camera stream from remcam-server.
//!
//! Frames can be fed to a v4l2loopback device (`--sink`), so local apps see
//! the remote camera as a normal webcam, and/or saved to files (`--out`).
//! They travel as the camera's own MJPEG or as H.264 with a bitrate that
//! adapts to the link (`--codec`).
//! Camera controls can be set at startup (`--ctrl`) or typed while streaming.
//! If the connection drops after streaming has started, the client keeps the
//! loopback device open and reconnects.
//!
//! The client has its own certificate, and the server must know it (pair
//! with `--pair`). Servers it trusts are kept in `known_servers`.

mod controls;
mod discover;
mod sink;

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use clap::{Parser, ValueEnum};
use remcam_proto::pairing::{self, Pake, Role};
use remcam_proto::tls::{self, ServerTrust};
use remcam_proto::trust::{self, Entry, KnownServers};
use remcam_proto::{
    ClientMsg, Codec, ControlInfo, ControlState, DeviceInfo, FourCC, FrameHeader, MAX_FRAME_LEN,
    PROTOCOL_VERSION, SERVER_NAME, ServerMsg, StreamOptions, VideoFormat, read_msg, write_msg,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::controls::Command;

/// How long to listen for mDNS advertisements.
const DISCOVERY_TIME: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// Size used when `--size` isn't given, if the camera offers it.
const PREFERRED_SIZE: (u32, u32) = (1280, 720);

#[derive(Parser)]
#[command(about = "Receive a webcam stream from a remcam server")]
struct Args {
    /// Server as host, host:port, or mDNS name. Omit to use the only server
    /// found on the LAN.
    server: Option<String>,

    /// List servers advertised on the LAN and exit.
    #[arg(long)]
    discover: bool,

    /// List the server's cameras, formats and controls, and exit.
    #[arg(long)]
    list: bool,

    /// Camera to open (e.g. /dev/video0). Defaults to the server's first camera.
    #[arg(long)]
    device: Option<String>,

    /// Frame size, WIDTHxHEIGHT. Defaults to 1280x720, or the largest size offered.
    #[arg(long, value_parser = parse_size)]
    size: Option<(u32, u32)>,

    /// Frame rate. Defaults to the highest the camera offers at that size.
    #[arg(long)]
    fps: Option<u32>,

    /// How frames travel: h264 adapts its bitrate to the link (Wi-Fi, WAN);
    /// mjpeg passes the camera's frames through (wired LAN, lowest latency,
    /// 20-60 Mbit/s).
    #[arg(long, value_enum, default_value_t = CodecArg::H264)]
    codec: CodecArg,

    /// Maximum H.264 bitrate in kbit/s. Defaults to 0.1 bits per pixel,
    /// e.g. 2765 for 1280x720 at 30 fps.
    #[arg(long, value_name = "KBPS")]
    bitrate: Option<u32>,

    /// Set a camera control after opening, e.g. --ctrl brightness=10. Repeatable.
    #[arg(long = "ctrl", value_name = "NAME=VALUE")]
    ctrls: Vec<String>,

    /// Pair with the server using the code shown by `remcam-server pair`,
    /// then exit. Afterwards both sides know each other.
    #[arg(long, value_name = "CODE", conflicts_with_all = ["fingerprint", "insecure", "list"])]
    pair: Option<String>,

    /// Expected SHA-256 fingerprint of the server certificate (printed by
    /// the server at startup). Only needed for a server not yet in
    /// known_servers; it is saved there once the connection works.
    #[arg(long)]
    fingerprint: Option<String>,

    /// Skip server authentication. The connection is still encrypted, and
    /// the server still has to know this client.
    #[arg(long, conflicts_with = "fingerprint")]
    insecure: bool,

    /// Name this client gives the server. Defaults to the hostname.
    #[arg(long)]
    name: Option<String>,

    /// Directory holding the client certificate (in client/) and
    /// known_servers. Defaults to $XDG_CONFIG_HOME/remcam or ~/.config/remcam.
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Feed frames to this v4l2loopback device (e.g. /dev/video10).
    #[arg(long, value_name = "DEVICE")]
    sink: Option<String>,

    /// Save MJPEG frames as DIR/<seq>.jpg, or H.264 as DIR/stream.h264.
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,

    /// Stop after receiving this many frames.
    #[arg(long)]
    frames: Option<u64>,

    /// Seconds between statistics lines; 0 disables them.
    #[arg(long, default_value_t = 5)]
    stats: u64,
}

#[derive(Clone, Copy, ValueEnum)]
enum CodecArg {
    H264,
    Mjpeg,
}

impl From<CodecArg> for Codec {
    fn from(c: CodecArg) -> Self {
        match c {
            CodecArg::H264 => Codec::H264,
            CodecArg::Mjpeg => Codec::Mjpeg,
        }
    }
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
    Ok((
        w.parse().map_err(|_| "bad width")?,
        h.parse().map_err(|_| "bad height")?,
    ))
}

/// Where to connect, and what mDNS claimed about it (if it came from there).
struct Target {
    /// What the server is called in known_servers: the host as given, or
    /// its mDNS name.
    name: String,
    addr: SocketAddr,
    advertised_fingerprint: Option<String>,
}

/// Why we accept the server's certificate.
enum TrustSource {
    /// `--fingerprint`; saved to known_servers once the connection works.
    Flag,
    /// The known_servers entry for this name.
    Known(Entry),
    /// Not known under this name, but under another (e.g. by address
    /// instead of hostname), and it's clear which server that is; the new
    /// name is saved once connected.
    AnyKnown,
    /// `--insecure`.
    Insecure,
}

struct Trust {
    tls: ServerTrust,
    source: TrustSource,
}

/// State that outlives a single connection.
struct State {
    sink: Option<sink::Sink>,
    received: u64,
    /// Whether any session got as far as streaming. Until then, errors are
    /// treated as configuration mistakes and end the program.
    streamed: bool,
    known: KnownServers,
    client_name: String,
}

enum End {
    Quit,
}

/// An error that reconnecting won't fix, such as a rejected certificate.
#[derive(Debug)]
struct Fatal(String);

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Fatal {}

fn is_fatal(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Fatal>())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let config_dir = match &args.config_dir {
        Some(d) => d.clone(),
        None => trust::default_config_dir()?,
    };
    let known = KnownServers::load(&config_dir)?;

    if args.discover {
        let servers = discover::browse(DISCOVERY_TIME).await?;
        if servers.is_empty() {
            println!("no remcam servers found");
        }
        for s in servers {
            let addrs: Vec<_> = s.addrs.iter().map(|a| a.to_string()).collect();
            println!("{}  port {}  [{}]", s.name, s.port, addrs.join(", "));
            println!(
                "    fingerprint {}  ({})",
                s.fingerprint.as_deref().unwrap_or("?"),
                trust_status(&known, &s)
            );
        }
        return Ok(());
    }

    let target = find_server(args.server.as_deref()).await?;
    // Separate from the server's identity, so both can run as one user.
    let identity = tls::load_or_create_identity(&config_dir.join("client"))?;
    let client_fp = identity.fingerprint();
    // Sanitized as the server stores it, so both sides show the same name.
    let client_name = trust::sanitize_name(&args.name.clone().unwrap_or_else(hostname));

    let bind: SocketAddr = if target.addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    }
    .parse()?;
    let mut endpoint = quinn::Endpoint::client(bind)?;

    if let Some(code) = &args.pair {
        let code = pairing::normalize_code(code)?;
        endpoint.set_default_client_config(tls::client_config(ServerTrust::Any, identity)?);
        let mut known = known;
        let result = pair(
            &endpoint,
            &target,
            &code,
            &client_name,
            &client_fp,
            &mut known,
        )
        .await;
        endpoint.wait_idle().await;
        return result;
    }

    let trust = server_trust(&args, &target, &known)?;
    endpoint.set_default_client_config(tls::client_config(trust.tls.clone(), identity)?);

    // Ctrl-C or SIGTERM (e.g. from systemd or `timeout`) end the stream
    // cleanly, so the server releases the camera right away instead of
    // waiting for the connection to time out.
    let (quit_tx, mut quit_rx) = watch::channel(false);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        let _ = quit_tx.send(true);
    });
    let mut commands = spawn_command_reader();

    let mut state = State {
        sink: None,
        received: 0,
        streamed: false,
        known,
        client_name,
    };
    let mut backoff = Duration::from_secs(1);
    let result = loop {
        let started = Instant::now();
        let outcome = session(
            &endpoint,
            &target,
            &trust,
            &args,
            &mut state,
            &mut quit_rx,
            &mut commands,
        )
        .await;
        match outcome {
            Ok(End::Quit) => break Ok(()),
            Err(e) if !state.streamed || is_fatal(&e) => break Err(e),
            Err(e) => {
                // A session that streamed for a while was healthy; start over.
                if started.elapsed() > MAX_BACKOFF {
                    backoff = Duration::from_secs(1);
                }
                warn!("{e:#}; reconnecting in {}s", backoff.as_secs());
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = quit_rx.changed() => break Ok(()),
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    };
    if let Some(sink) = state.sink.take() {
        sink.close();
    }
    endpoint.wait_idle().await;
    result
}

async fn find_server(server: Option<&str>) -> anyhow::Result<Target> {
    if let Some(name) = server {
        if let Ok(addr) = resolve(name).await {
            return Ok(Target {
                name: name.to_string(),
                addr,
                advertised_fingerprint: None,
            });
        }
        // Not a resolvable host: try it as an mDNS instance name.
        let servers = discover::browse(DISCOVERY_TIME).await?;
        let found = servers
            .into_iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .with_context(|| {
                format!(
                    "could not resolve {name:?}, and no remcam server by that name is on the LAN"
                )
            })?;
        return target_from(found);
    }
    let mut servers = discover::browse(DISCOVERY_TIME).await?;
    match servers.len() {
        0 => bail!("no remcam servers found on the LAN; pass a server address"),
        1 => target_from(servers.remove(0)),
        _ => {
            let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
            bail!(
                "several servers found ({}); pass one by name",
                names.join(", ")
            )
        }
    }
}

fn target_from(server: discover::Server) -> anyhow::Result<Target> {
    if server.version.is_some_and(|v| v != PROTOCOL_VERSION) {
        warn!(
            "{} advertises protocol version {:?}",
            server.name, server.version
        );
    }
    let addr = server
        .addr()
        .with_context(|| format!("{} has no usable address", server.name))?;
    info!("found {} at {addr}", server.name);
    Ok(Target {
        name: server.name,
        addr,
        advertised_fingerprint: server.fingerprint,
    })
}

/// How a discovered server relates to known_servers, for `--discover`.
fn trust_status(known: &KnownServers, server: &discover::Server) -> String {
    let advertised = server.fingerprint.as_deref();
    match (known.lookup(&server.name), advertised) {
        // Without an advertised fingerprint there's nothing to compare.
        (Some(_), None) => "known (no fingerprint advertised)".into(),
        (Some(e), Some(fp)) if e.fingerprint == fp => "known".into(),
        (Some(e), Some(_)) => format!(
            "known, but {}:{} has a different fingerprint",
            known.path().display(),
            e.line
        ),
        (None, Some(fp)) if known.contains_fingerprint(fp) => "known under another name".into(),
        (None, _) => "not paired".into(),
    }
}

/// Decide which server certificates to accept, before connecting.
fn server_trust(args: &Args, target: &Target, known: &KnownServers) -> anyhow::Result<Trust> {
    if args.insecure {
        warn!("--insecure: server identity is NOT verified");
        return Ok(Trust {
            tls: ServerTrust::Any,
            source: TrustSource::Insecure,
        });
    }
    let entry = known.lookup(&target.name);
    if let Some(fp) = &args.fingerprint {
        let fp = tls::parse_fingerprint(fp)?;
        if let Some(e) = entry.filter(|e| e.fingerprint != fp) {
            bail!(
                "{}:{} has a different fingerprint for {}:\n  {}\n\
                 If the server's identity changed on purpose, pair again (or delete that line).",
                known.path().display(),
                e.line,
                target.name,
                e.fingerprint
            );
        }
        return Ok(Trust {
            tls: ServerTrust::Fingerprints(vec![fp]),
            source: TrustSource::Flag,
        });
    }
    if let Some(e) = entry {
        return Ok(Trust {
            tls: ServerTrust::Fingerprints(vec![e.fingerprint.clone()]),
            source: TrustSource::Known(e.clone()),
        });
    }
    // Not known under this name. Accept a server known under another name
    // only when it's clear which one is meant, since the new name gets saved
    // for it: the one it advertises over mDNS (TLS still has to prove it), or
    // the only known server. Accepting any known server would let whichever
    // answers at this address claim the name.
    let fingerprints = known.fingerprints();
    let fingerprint = match &target.advertised_fingerprint {
        Some(fp) if known.contains_fingerprint(fp) => fp.clone(),
        _ => match fingerprints.as_slice() {
            [] => bail!(Fatal(not_known_message(target))),
            [fp] => fp.clone(),
            _ => bail!(Fatal(format!(
                "{} isn't in {}, and several servers are, so it's unclear which one this is.\n\
                 Connect using the name it's saved under, pass --fingerprint, or pair with it.",
                target.name,
                known.path().display()
            ))),
        },
    };
    Ok(Trust {
        tls: ServerTrust::Fingerprints(vec![fingerprint]),
        source: TrustSource::AnyKnown,
    })
}

fn not_known_message(target: &Target) -> String {
    let mut msg = format!(
        "{} isn't a known server. To pair with it, run `remcam-server pair` on the server, then\n    \
         remcam-client {} --pair CODE\n\
         or pass --fingerprint (the server prints it at startup).",
        target.name, target.name
    );
    if let Some(fp) = &target.advertised_fingerprint {
        msg += &format!(
            "\nIt advertises fingerprint {fp} on the LAN, but that isn't proof: \
             compare it with the server's own output."
        );
    }
    msg
}

/// Turn handshake failures into messages that say what to do.
fn connect_error(e: quinn::ConnectionError, target: &Target, trust: &Trust) -> anyhow::Error {
    use quinn::{ConnectionError, TransportErrorCode};
    const NO_APPLICATION_PROTOCOL: u8 = 120;
    match &e {
        ConnectionError::ConnectionClosed(c)
            if c.error_code == TransportErrorCode::crypto(NO_APPLICATION_PROTOCOL) =>
        {
            Fatal(format!(
                "{} doesn't speak remcam protocol v{PROTOCOL_VERSION}; \
                 run the same remcam version on server and client",
                target.addr
            ))
            .into()
        }
        // The server's limits on new connections; retrying later is fine.
        ConnectionError::ConnectionClosed(c)
            if c.error_code == TransportErrorCode::CONNECTION_REFUSED =>
        {
            anyhow::anyhow!(
                "{} refused the connection: too many recent attempts from this address, \
                 or too many clients connecting at once; try again in a minute",
                target.addr
            )
        }
        // TLS errors raised here: our check of the server's certificate.
        ConnectionError::TransportError(t) if (0x100..0x200).contains(&u64::from(t.code)) => {
            let hint = match &trust.source {
                TrustSource::Known(e) => format!(
                    "known_servers line {} expects {} for {}.\n\
                     If the server's identity changed on purpose (reinstalled, config deleted), \
                     pair again. Otherwise something may be impersonating it.",
                    e.line, e.fingerprint, target.name
                ),
                TrustSource::Flag => "It doesn't match --fingerprint.".into(),
                TrustSource::AnyKnown => not_known_message(target),
                TrustSource::Insecure => String::new(),
            };
            let reason = t.reason.trim_start_matches("unexpected error: ");
            Fatal(format!("{}: {reason}\n{hint}", target.addr)).into()
        }
        _ => anyhow::Error::new(e).context(format!("connecting to {}", target.addr)),
    }
}

async fn connect(
    endpoint: &quinn::Endpoint,
    target: &Target,
    trust: Option<&Trust>,
) -> anyhow::Result<quinn::Connection> {
    let connecting = endpoint.connect(target.addr, SERVER_NAME)?;
    match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
        Err(_) => bail!("timed out connecting to {}", target.addr),
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => match trust {
            Some(trust) => Err(connect_error(e, target, trust)),
            None => Err(anyhow::Error::new(e).context(format!("connecting to {}", target.addr))),
        },
    }
}

/// Pair with the server: prove we both know the code on this connection,
/// then remember its certificate. The server remembers ours.
async fn pair(
    endpoint: &quinn::Endpoint,
    target: &Target,
    code: &str,
    client_name: &str,
    client_fp: &str,
    known: &mut KnownServers,
) -> anyhow::Result<()> {
    let conn = connect(endpoint, target, None).await?;
    let server_fp = tls::peer_fingerprint(&conn).context("server presented no certificate")?;
    let result = async {
        let (mut ctl_tx, mut ctl_rx) = conn.open_bi().await?;
        let (pake, spake) = Pake::start(Role::Client, code);
        write_msg(
            &mut ctl_tx,
            &ClientMsg::Pair {
                version: PROTOCOL_VERSION,
                client_name: client_name.to_string(),
                spake,
            },
        )
        .await?;
        let (server_name, server_spake) = match read_msg(&mut ctl_rx).await? {
            ServerMsg::Pair { server_name, spake } => (server_name, spake),
            ServerMsg::Error { msg } => bail!("the server refused: {msg}"),
            other => bail!("expected Pair, got {other:?}"),
        };
        let binding = pairing::binding(&conn, client_fp, &server_fp)?;
        let confirmation = pake.finish(&server_spake, binding)?;
        write_msg(
            &mut ctl_tx,
            &ClientMsg::PairConfirm {
                mac: confirmation.mac(),
            },
        )
        .await?;
        match read_msg(&mut ctl_rx).await? {
            ServerMsg::PairConfirm { mac } if confirmation.verify(&mac) => {}
            ServerMsg::PairConfirm { .. } => bail!(
                "the server's confirmation doesn't match, so something may be \
                 intercepting the connection. Nothing was saved."
            ),
            ServerMsg::Error { msg } => bail!("pairing failed: {msg}"),
            other => bail!("expected PairConfirm, got {other:?}"),
        }
        if let Some(old) = known
            .lookup(&target.name)
            .filter(|e| e.fingerprint != server_fp)
        {
            warn!(
                "replacing the old fingerprint for {} ({})",
                target.name, old.fingerprint
            );
        }
        known.replace(&target.name, &server_fp)?;
        println!("Paired with {server_name} as {client_name}.");
        println!("    server fingerprint {server_fp}");
        println!(
            "Saved to {}. From now on, connect with:",
            known.path().display()
        );
        println!("    remcam-client {} [options]", target.name);
        anyhow::Ok(())
    }
    .await;
    conn.close(0u32.into(), b"bye");
    result
}

/// Read interactive commands from stdin, if it's a terminal.
///
/// This uses a plain thread rather than `tokio::io::stdin`: tokio reads stdin
/// on a runtime worker that the runtime waits for at shutdown, so the
/// process would hang on exit until another line was typed. A detached
/// thread doesn't hold up exit. When stdin isn't a terminal the channel is
/// simply closed, which `select!` treats as a disabled branch.
fn spawn_command_reader() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(16);
    if std::io::stdin().is_terminal() {
        eprintln!("type 'help' for commands");
        std::thread::spawn(move || {
            for line in std::io::stdin().lines() {
                let Ok(line) = line else { break };
                if tx.blocking_send(line).is_err() {
                    break;
                }
            }
        });
    }
    rx
}

async fn session(
    endpoint: &quinn::Endpoint,
    target: &Target,
    trust: &Trust,
    args: &Args,
    state: &mut State,
    quit: &mut watch::Receiver<bool>,
    commands: &mut mpsc::Receiver<String>,
) -> anyhow::Result<End> {
    let conn = connect(endpoint, target, Some(trust)).await?;
    // A server accepted by --fingerprint or under another name: remember
    // it under this name too, so next time no flag is needed.
    if matches!(trust.source, TrustSource::Flag | TrustSource::AnyKnown)
        && state.known.lookup(&target.name).is_none()
        && let Some(fp) = tls::peer_fingerprint(&conn)
    {
        state.known.add(&target.name, &fp)?;
        info!("saved {} to {}", target.name, state.known.path().display());
    }
    let result = stream(&conn, target, args, state, quit, commands).await;
    conn.close(0u32.into(), b"bye");
    result
}

async fn stream(
    conn: &quinn::Connection,
    target: &Target,
    args: &Args,
    state: &mut State,
    quit: &mut watch::Receiver<bool>,
    commands: &mut mpsc::Receiver<String>,
) -> anyhow::Result<End> {
    let (mut ctl_tx, mut ctl_rx) = conn.open_bi().await?;
    write_msg(
        &mut ctl_tx,
        &ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            client_name: state.client_name.clone(),
        },
    )
    .await?;

    let codecs = match read_msg(&mut ctl_rx).await? {
        ServerMsg::Hello {
            version,
            server_name,
            codecs,
        } if version == PROTOCOL_VERSION => {
            info!("connected to {server_name} ({})", conn.remote_address());
            codecs
        }
        ServerMsg::Hello { version, .. } => bail!("server speaks protocol version {version}"),
        ServerMsg::Unauthorized { fingerprint } => bail!(Fatal(format!(
            "{} doesn't know this client (fingerprint {fingerprint}).\n\
             To pair, run `remcam-server pair` on the server, then\n    \
             remcam-client {} --pair CODE\n\
             or add the fingerprint to the server's authorized_clients.",
            target.name, target.name
        ))),
        ServerMsg::Error { msg } => bail!("server error: {msg}"),
        other => bail!("expected Hello, got {other:?}"),
    };
    let ServerMsg::DeviceList { devices } = read_msg(&mut ctl_rx).await? else {
        bail!("expected DeviceList");
    };
    if args.list {
        for dev in &devices {
            print!("{}", dev.describe());
        }
        return Ok(End::Quit);
    }
    let device = pick_device(&devices, args.device.as_deref())?;
    let format = pick_format(device, args.size, args.fps)?;
    let codec = Codec::from(args.codec);
    if !codecs.contains(&codec) {
        bail!("server can't send {codec}");
    }

    write_msg(
        &mut ctl_tx,
        &ClientMsg::Open {
            device_id: device.id.clone(),
            format,
            stream: StreamOptions {
                codec,
                max_bitrate: args.bitrate.map(|k| k.saturating_mul(1000)),
            },
        },
    )
    .await?;
    let (epoch, actual, codec) = match read_msg(&mut ctl_rx).await? {
        ServerMsg::Opened {
            epoch,
            format,
            stream,
        } => {
            let rate = stream
                .max_bitrate
                .map(|b| format!(", up to {:.2} Mbit/s", b as f64 / 1e6))
                .unwrap_or_default();
            info!(
                "streaming {} ({}) {format} as {}{rate}",
                device.id, device.name, stream.codec
            );
            (epoch, format, stream.codec)
        }
        ServerMsg::Error { msg } => bail!("server could not open {}: {msg}", device.id),
        other => bail!("expected Opened, got {other:?}"),
    };
    let mut controls = device.controls.clone();

    if let Some(path) = &args.sink {
        // Keep the loopback device across reconnects unless the size changed,
        // so apps reading from it don't lose the camera.
        if state.sink.as_ref().is_some_and(|s| s.format != actual) {
            state.sink.take().unwrap().close();
        }
        if state.sink.is_none() {
            state.sink = Some(sink::Sink::start(path, actual)?);
        }
    }
    // H.264 frames only make sense together, so they go into one file.
    let mut h264_out = None;
    if let Some(dir) = &args.out {
        tokio::fs::create_dir_all(dir).await?;
        if codec == Codec::H264 {
            let path = dir.join("stream.h264");
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .with_context(|| format!("opening {}", path.display()))?;
            h264_out = Some(file);
        }
    }

    for assignment in &args.ctrls {
        let (name, value) = controls::parse_assignment(assignment)?;
        let info = controls::find(&controls, name)?;
        let value = controls::parse_value(info, value)?;
        write_msg(&mut ctl_tx, &ClientMsg::SetControl { id: info.id, value }).await?;
    }
    state.streamed = true;

    // Server messages after Opened: control updates and errors.
    let (event_tx, mut events) = mpsc::channel(16);
    tokio::spawn(async move {
        while let Ok(msg) = read_msg::<_, ServerMsg>(&mut ctl_rx).await {
            if event_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let (frame_tx, mut frames) = mpsc::channel(8);
    let stale = Arc::new(AtomicU64::new(0));
    match codec {
        Codec::Mjpeg => tokio::spawn(accept_newest(conn.clone(), epoch, frame_tx, stale.clone())),
        Codec::H264 => tokio::spawn(accept_in_order(conn.clone(), epoch, frame_tx)),
    };

    let sink_dropped = |state: &State| {
        state
            .sink
            .as_ref()
            .map_or(0, |s| s.dropped.swap(0, Ordering::Relaxed))
    };
    let mut stats = Stats::new(codec);
    let stats_every = Duration::from_secs(args.stats.max(1));
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + stats_every, stats_every);

    let end = loop {
        tokio::select! {
            frame = frames.recv() => {
                let (header, data) = match frame {
                    Some(Received::Frame(header, data)) => (header, data),
                    Some(Received::Lost) => {
                        if let Some(sink) = &state.sink {
                            sink.mark_lost();
                        }
                        continue;
                    }
                    None => bail!("connection lost"),
                };
                stats.record(&header, data.len());
                if let Some(file) = &mut h264_out {
                    file.write_all(&data).await?;
                } else if let Some(dir) = &args.out {
                    tokio::fs::write(dir.join(format!("{:08}.jpg", header.seq)), &data).await?;
                }
                if let Some(sink) = &state.sink {
                    sink.push(sink::Frame {
                        epoch,
                        codec,
                        keyframe: header.keyframe,
                        data,
                    })?;
                    if sink.take_keyframe_request() {
                        write_msg(&mut ctl_tx, &ClientMsg::RequestKeyframe).await?;
                    }
                }
                state.received += 1;
                if args.frames.is_some_and(|n| state.received >= n) {
                    break End::Quit;
                }
            }
            event = events.recv() => match event {
                Some(ServerMsg::Controls { controls: states }) => apply_states(&mut controls, &states),
                Some(ServerMsg::Error { msg }) => warn!("server: {msg}"),
                Some(other) => debug!("unexpected message {other:?}"),
                None => bail!("server closed the connection"),
            },
            Some(line) = commands.recv() => {
                match run_command(&line, &controls, &mut ctl_tx).await {
                    Ok(Some(end)) => break end,
                    Ok(None) => {}
                    Err(e) => warn!("{e:#}"),
                }
            }
            _ = tick.tick(), if args.stats > 0 => {
                stats.report(conn, stale.swap(0, Ordering::Relaxed), sink_dropped(state));
            }
            _ = quit.changed() => break End::Quit,
        }
    };
    if args.stats > 0 {
        stats.report(conn, stale.swap(0, Ordering::Relaxed), sink_dropped(state));
    }
    write_msg(&mut ctl_tx, &ClientMsg::Close).await?;
    ctl_tx.finish()?;
    Ok(end)
}

async fn run_command(
    line: &str,
    controls: &[ControlInfo],
    ctl_tx: &mut quinn::SendStream,
) -> anyhow::Result<Option<End>> {
    let (info, value) = match controls::parse_command(line)? {
        None => return Ok(None),
        Some(Command::Quit) => return Ok(Some(End::Quit)),
        Some(Command::Help) => {
            println!("{}", controls::HELP);
            return Ok(None);
        }
        Some(Command::List) => {
            for c in controls {
                println!("{}", c.describe());
            }
            return Ok(None);
        }
        Some(Command::Set { name, value }) => {
            let info = controls::find(controls, &name)?;
            (info, controls::parse_value(info, &value)?)
        }
        Some(Command::Reset { name }) => {
            let info = controls::find(controls, &name)?;
            (info, info.default)
        }
    };
    if info.state.inactive {
        warn!(
            "{} is inactive right now; the camera may ignore it",
            info.name
        );
    }
    write_msg(ctl_tx, &ClientMsg::SetControl { id: info.id, value }).await?;
    Ok(None)
}

/// Update local control state from the server and log what changed.
fn apply_states(controls: &mut [ControlInfo], states: &[ControlState]) {
    for new in states {
        let Some(c) = controls.iter_mut().find(|c| c.id == new.id) else {
            continue;
        };
        if c.state != *new {
            let note = if new.inactive { " (inactive)" } else { "" };
            info!("{} = {}{note}", c.name, c.format_value(new.value));
            c.state = *new;
        }
    }
}

fn pick_device<'a>(
    devices: &'a [DeviceInfo],
    wanted: Option<&str>,
) -> anyhow::Result<&'a DeviceInfo> {
    match wanted {
        Some(id) => devices.iter().find(|d| d.id == id).with_context(|| {
            let ids: Vec<_> = devices.iter().map(|d| d.id.as_str()).collect();
            format!("server has no camera {id}; it has: {}", ids.join(", "))
        }),
        None => devices.first().context("server has no cameras"),
    }
}

/// Choose an MJPEG size and frame rate the camera actually offers, so a typo
/// fails here with a useful message instead of the driver silently picking
/// something else.
fn pick_format(
    device: &DeviceInfo,
    size: Option<(u32, u32)>,
    fps: Option<u32>,
) -> anyhow::Result<VideoFormat> {
    let sizes = device
        .formats
        .iter()
        .find(|f| f.fourcc == FourCC::MJPG)
        .map(|f| f.sizes.as_slice())
        .unwrap_or_default();
    let available = || {
        sizes
            .iter()
            .map(|s| format!("{}x{}", s.width, s.height))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let chosen = match size {
        Some((w, h)) => sizes
            .iter()
            .find(|s| (s.width, s.height) == (w, h))
            .with_context(|| {
                format!(
                    "{} doesn't offer {w}x{h}; sizes: {}",
                    device.id,
                    available()
                )
            })?,
        None => sizes
            .iter()
            .find(|s| (s.width, s.height) == PREFERRED_SIZE)
            .or_else(|| sizes.iter().max_by_key(|s| s.width * s.height))
            .with_context(|| format!("{} offers no MJPEG sizes", device.id))?,
    };
    let rate = match fps {
        Some(f) if chosen.fps.is_empty() || chosen.fps.contains(&f) => f,
        Some(f) => bail!(
            "{}x{} doesn't offer {f} fps; rates: {:?}",
            chosen.width,
            chosen.height,
            chosen.fps
        ),
        None => chosen.fps.first().copied().unwrap_or(30),
    };
    Ok(VideoFormat {
        fourcc: FourCC::MJPG,
        width: chosen.width,
        height: chosen.height,
        fps: rate,
    })
}

enum Received {
    Frame(FrameHeader, Vec<u8>),
    /// A frame stream ended early (H.264 only), so the decoder must resync.
    Lost,
}

/// MJPEG: accept one unidirectional stream per frame and forward complete
/// frames in order of arrival. A frame that completes after a newer one has
/// already been delivered is stale and discarded.
async fn accept_newest(
    conn: quinn::Connection,
    epoch: u32,
    tx: mpsc::Sender<Received>,
    stale: Arc<AtomicU64>,
) {
    let newest = Arc::new(AtomicU64::new(0));
    while let Ok(mut stream) = conn.accept_uni().await {
        let (tx, newest, stale) = (tx.clone(), newest.clone(), stale.clone());
        tokio::spawn(async move {
            let header: FrameHeader = match read_msg(&mut stream).await {
                Ok(h) => h,
                Err(e) => return debug!("bad frame header: {e:#}"),
            };
            if header.epoch != epoch || header.seq < newest.load(Ordering::Relaxed) {
                stale.fetch_add(1, Ordering::Relaxed);
                let _ = stream.stop(0u32.into());
                return;
            }
            let data = match stream.read_to_end(MAX_FRAME_LEN).await {
                Ok(d) => d,
                Err(e) => return debug!("frame {} incomplete: {e}", header.seq),
            };
            if data.len() != header.len as usize {
                return warn!(
                    "frame {}: expected {} bytes, got {}",
                    header.seq,
                    header.len,
                    data.len()
                );
            }
            if newest.fetch_max(header.seq, Ordering::Relaxed) > header.seq {
                stale.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let _ = tx.send(Received::Frame(header, data)).await;
        });
    }
}

/// H.264: every frame is needed to decode the next, so frames are delivered
/// in stream order (which the server keeps equal to frame order). Streams
/// are still read concurrently, so a slow frame doesn't hold up reading the
/// ones behind it, only delivering them.
async fn accept_in_order(conn: quinn::Connection, epoch: u32, tx: mpsc::Sender<Received>) {
    let (order_tx, mut order_rx) = mpsc::channel::<tokio::task::JoinHandle<Option<Received>>>(32);
    tokio::spawn(async move {
        while let Some(reader) = order_rx.recv().await {
            let received = match reader.await {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(_) => Received::Lost,
            };
            if tx.send(received).await.is_err() {
                break;
            }
        }
    });
    while let Ok(mut stream) = conn.accept_uni().await {
        let reader = tokio::spawn(async move {
            let header: FrameHeader = match read_msg(&mut stream).await {
                Ok(h) => h,
                Err(e) => {
                    debug!("bad frame header: {e:#}");
                    return Some(Received::Lost);
                }
            };
            if header.epoch != epoch {
                // Left over from an earlier Open; not part of this stream.
                let _ = stream.stop(0u32.into());
                return None;
            }
            let data = match stream.read_to_end(MAX_FRAME_LEN).await {
                Ok(d) => d,
                Err(e) => {
                    debug!("frame {} incomplete: {e}", header.seq);
                    return Some(Received::Lost);
                }
            };
            if data.len() != header.len as usize {
                warn!("frame {}: length mismatch", header.seq);
                return Some(Received::Lost);
            }
            Some(Received::Frame(header, data))
        });
        if order_tx.send(reader).await.is_err() {
            break;
        }
    }
}

struct Stats {
    codec: Codec,
    since: Instant,
    frames: u64,
    bytes: u64,
    latency_sum_us: i64,
    latency_max_us: i64,
    last_seq: Option<u64>,
    /// Sequence numbers never delivered: dropped by the server, lost, or stale.
    missing: u64,
    keyframes: u64,
}

impl Stats {
    fn new(codec: Codec) -> Self {
        Self {
            codec,
            since: Instant::now(),
            frames: 0,
            bytes: 0,
            latency_sum_us: 0,
            latency_max_us: 0,
            last_seq: None,
            missing: 0,
            keyframes: 0,
        }
    }

    fn record(&mut self, header: &FrameHeader, len: usize) {
        let latency = now_us() as i64 - header.capture_ts_us as i64;
        self.frames += 1;
        self.bytes += len as u64;
        self.latency_sum_us += latency;
        self.latency_max_us = self.latency_max_us.max(latency);
        self.keyframes += u64::from(header.keyframe);
        if let Some(last) = self.last_seq {
            self.missing += header.seq.saturating_sub(last + 1);
        }
        self.last_seq = Some(header.seq);
    }

    /// Log and reset the per-interval counters. Latency compares the server's
    /// and the client's wall clocks, so it is only meaningful when both are
    /// NTP-synced (or the same machine).
    fn report(&mut self, conn: &quinn::Connection, stale: u64, sink_dropped: u64) {
        let secs = self.since.elapsed().as_secs_f64().max(1e-3);
        let avg_ms = if self.frames > 0 {
            self.latency_sum_us as f64 / self.frames as f64 / 1000.0
        } else {
            0.0
        };
        let keyframes = match self.codec {
            Codec::H264 => format!("  keyframes {}", self.keyframes),
            Codec::Mjpeg => String::new(),
        };
        info!(
            "{:>5.1} fps  {:>6.2} Mbit/s  latency avg {:>6.1} ms max {:>6.1} ms  rtt {:>5.1} ms  missing {} (stale {})  sink dropped {}{keyframes}",
            self.frames as f64 / secs,
            self.bytes as f64 * 8.0 / 1e6 / secs,
            avg_ms,
            self.latency_max_us as f64 / 1000.0,
            conn.rtt().as_secs_f64() * 1000.0,
            self.missing,
            stale,
            sink_dropped,
        );
        *self = Stats {
            last_seq: self.last_seq,
            ..Stats::new(self.codec)
        };
    }
}

async fn resolve(server: &str) -> anyhow::Result<SocketAddr> {
    let port = remcam_proto::DEFAULT_PORT;
    if let Ok(addr) = server.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = server.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let with_port = match server.rsplit_once(':') {
        Some((_, p)) if p.parse::<u16>().is_ok() => server.to_string(),
        _ => format!("{server}:{port}"),
    };
    tokio::net::lookup_host(&with_port)
        .await
        .with_context(|| format!("resolving {with_port}"))?
        .next()
        .with_context(|| format!("no address for {with_port}"))
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "remcam-client".into())
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
