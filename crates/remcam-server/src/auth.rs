//! Who may use the server: `authorized_clients`, and pairing new clients.
//!
//! `remcam-server pair` talks to the running server over a Unix socket that
//! only the same user can use (see [`socket_path`]). The server creates a one-time code,
//! the command shows it and waits; the outcome of each attempt is reported
//! back. The code lives as long as the command does, at most [`CODE_TTL`].
//!
//! Socket protocol, one line per message:
//! ```text
//! → pair
//! ← code K7QM-4XRT 300          (seconds valid)
//! ← wrong 2                     (a client tried a wrong code; attempts left)
//! ← paired <fingerprint> laptop
//! ← failed <reason>             (the right code, but saving the client failed)
//! ← cancelled                   (too many wrong attempts)
//! ← replaced                    (a newer `pair` took over)
//! ```

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use remcam_proto::pairing::{self, Pake, Role};
use remcam_proto::trust::AuthorizedClients;
use remcam_proto::{ClientMsg, ServerMsg, read_msg, write_msg};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// How long a pairing code stays valid.
pub const CODE_TTL: Duration = Duration::from_secs(300);
/// Wrong attempts before a code is cancelled.
const MAX_ATTEMPTS: u32 = 3;
/// Per message during pairing; the person typed the code before connecting.
const PAIR_STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// Where the server for `config_dir` listens for `remcam-server pair`:
/// `$XDG_RUNTIME_DIR` (private to the user) when set, named after the config
/// directory so several servers can coexist; otherwise the config directory.
pub fn socket_path(config_dir: &Path) -> anyhow::Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let path = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => {
            let dir = std::fs::canonicalize(config_dir).unwrap_or_else(|_| config_dir.into());
            let mut h = std::hash::DefaultHasher::new();
            dir.hash(&mut h);
            PathBuf::from(runtime).join(format!("remcam-server-{:016x}.sock", h.finish()))
        }
        None => config_dir.join("server.sock"),
    };
    // sockaddr_un holds 108 bytes, including the terminating NUL.
    if path.as_os_str().len() > 107 {
        bail!(
            "control socket path {} is too long; set XDG_RUNTIME_DIR or use a shorter --config-dir",
            path.display()
        );
    }
    Ok(path)
}

/// The user on the other end of a control socket must be us.
fn check_peer(stream: &UnixStream) -> anyhow::Result<()> {
    let uid = unsafe { libc::geteuid() };
    let peer = stream.peer_cred()?.uid();
    if peer != uid {
        bail!("control socket belongs to uid {peer}, not us ({uid})");
    }
    Ok(())
}

pub struct Auth {
    pub clients: AuthorizedClients,
    pub fingerprint: String,
    pub server_name: String,
    pending: Mutex<Option<PendingCode>>,
}

struct PendingCode {
    id: u64,
    code: String,
    attempts_left: u32,
    /// Set while a client is using the code, so attempts can't overlap.
    busy: bool,
    events: mpsc::UnboundedSender<String>,
}

/// A client's claim on the pending code. Unless [`Attempt::succeeded`] is
/// called, dropping it counts as a wrong attempt, so a client can't learn
/// anything by giving up halfway.
struct Attempt<'a> {
    auth: &'a Auth,
    id: u64,
    code: String,
    done: bool,
}

impl Auth {
    pub fn new(config_dir: &Path, fingerprint: String, server_name: String) -> Self {
        Self {
            clients: AuthorizedClients::new(config_dir),
            fingerprint,
            server_name,
            pending: Mutex::new(None),
        }
    }

    /// Create a code, replacing any earlier one. Events about it go to `events`.
    fn new_code(&self, events: mpsc::UnboundedSender<String>) -> anyhow::Result<(u64, String)> {
        let display = pairing::generate_code()?;
        let mut pending = self.pending.lock().unwrap();
        let id = pending.as_ref().map_or(1, |p| p.id + 1);
        if let Some(old) = pending.take() {
            let _ = old.events.send("replaced".into());
        }
        *pending = Some(PendingCode {
            id,
            code: pairing::normalize_code(&display)?,
            attempts_left: MAX_ATTEMPTS,
            busy: false,
            events,
        });
        Ok((id, display))
    }

    fn cancel(&self, id: u64) {
        let mut pending = self.pending.lock().unwrap();
        if pending.as_ref().is_some_and(|p| p.id == id) {
            *pending = None;
        }
    }

    fn begin_attempt(&self) -> Result<Attempt<'_>, &'static str> {
        let mut pending = self.pending.lock().unwrap();
        let p = pending
            .as_mut()
            .ok_or("no pairing code is active; run `remcam-server pair` on the server first")?;
        if p.busy {
            return Err("another client is pairing right now; try again in a moment");
        }
        p.busy = true;
        Ok(Attempt {
            auth: self,
            id: p.id,
            code: p.code.clone(),
            done: false,
        })
    }
}

impl Attempt<'_> {
    /// Use up the code (single use) for a client that proved it knows it,
    /// returning where to report the outcome. `None` if the code was
    /// cancelled, expired or replaced meanwhile: then the client must not be
    /// authorized.
    fn claim(mut self) -> Option<mpsc::UnboundedSender<String>> {
        self.done = true;
        let mut pending = self.auth.pending.lock().unwrap();
        if pending.as_ref().is_some_and(|p| p.id == self.id) {
            pending.take().map(|p| p.events)
        } else {
            None
        }
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut pending = self.auth.pending.lock().unwrap();
        let Some(p) = pending.as_mut().filter(|p| p.id == self.id) else {
            return;
        };
        p.busy = false;
        p.attempts_left -= 1;
        if p.attempts_left == 0 {
            let _ = p.events.send("cancelled".into());
            *pending = None;
        } else {
            let _ = p.events.send(format!("wrong {}", p.attempts_left));
        }
    }
}

/// Run the pairing exchange with a client that sent [`ClientMsg::Pair`].
pub async fn pair(
    auth: &Auth,
    conn: &quinn::Connection,
    ctl_tx: &mut quinn::SendStream,
    ctl_rx: &mut quinn::RecvStream,
    client_fp: &str,
    client_name: &str,
    client_spake: &[u8],
) -> anyhow::Result<()> {
    let attempt = match auth.begin_attempt() {
        Ok(a) => a,
        Err(msg) => {
            send_final(ctl_tx, &ServerMsg::Error { msg: msg.into() }).await;
            bail!("pairing attempt from {client_name}: {msg}");
        }
    };
    let (pake, spake) = Pake::start(Role::Server, &attempt.code);
    write_msg(
        ctl_tx,
        &ServerMsg::Pair {
            server_name: auth.server_name.clone(),
            spake,
        },
    )
    .await?;
    let binding = pairing::binding(conn, client_fp, &auth.fingerprint)?;
    let confirmation = pake.finish(client_spake, binding)?;

    let mac = match tokio::time::timeout(PAIR_STEP_TIMEOUT, read_msg(ctl_rx)).await {
        Ok(Ok(ClientMsg::PairConfirm { mac })) => mac,
        Ok(Ok(other)) => bail!("pairing: expected PairConfirm, got {other:?}"),
        Ok(Err(e)) => return Err(e.context("pairing")),
        Err(_) => bail!("pairing: client went quiet"),
    };
    if !confirmation.verify(&mac) {
        drop(attempt);
        let msg = "wrong pairing code".to_string();
        send_final(ctl_tx, &ServerMsg::Error { msg }).await;
        bail!("{client_name} ({client_fp}) tried a wrong pairing code");
    }

    let Some(events) = attempt.claim() else {
        let msg = "the pairing code was cancelled or expired meanwhile; run \
                   `remcam-server pair` again"
            .to_string();
        send_final(ctl_tx, &ServerMsg::Error { msg }).await;
        bail!("{client_name} ({client_fp}) used a pairing code that had just ended");
    };
    if let Err(e) = auth.clients.add(client_fp, client_name) {
        let reason = format!("{e:#}").replace('\n', " ");
        let _ = events.send(format!("failed {reason}"));
        let msg = "the server couldn't save the pairing".to_string();
        send_final(ctl_tx, &ServerMsg::Error { msg }).await;
        return Err(e.context("saving the new client"));
    }
    // A client paired before keeps the name it was first saved under.
    let name = auth
        .clients
        .lookup(client_fp)
        .ok()
        .flatten()
        .unwrap_or_else(|| client_name.to_string());
    let _ = events.send(format!("paired {client_fp} {name}"));
    info!(
        "paired with {name} ({client_fp}); it is in {}",
        auth.clients.path().display()
    );
    send_final(
        ctl_tx,
        &ServerMsg::PairConfirm {
            mac: confirmation.mac(),
        },
    )
    .await;
    Ok(())
}

/// Send the last message on the control stream and wait (briefly) until the
/// client has it, since closing the connection right away could lose it.
pub async fn send_final(ctl_tx: &mut quinn::SendStream, msg: &ServerMsg) {
    let delivered = async {
        write_msg(ctl_tx, msg).await?;
        ctl_tx.finish()?;
        let _ = ctl_tx.stopped().await;
        anyhow::Ok(())
    };
    let _ = tokio::time::timeout(Duration::from_secs(3), delivered).await;
}

/// Listen on the control socket for `remcam-server pair`.
pub fn listen(config_dir: &Path, auth: Arc<Auth>) -> anyhow::Result<()> {
    let path = socket_path(config_dir)?;
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        bail!(
            "another remcam-server is using {} (pairing goes to that one)",
            config_dir.display()
        );
    }
    // Left over from a server that didn't shut down cleanly.
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            // Belt and braces next to the file mode: only our own user.
            if let Err(e) = check_peer(&stream) {
                warn!("control socket: refusing a connection: {e:#}");
                continue;
            }
            let auth = auth.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_control(stream, &auth).await {
                    warn!("control socket: {e:#}");
                }
            });
        }
    });
    Ok(())
}

async fn serve_control(stream: UnixStream, auth: &Auth) -> anyhow::Result<()> {
    let (rx, mut tx) = stream.into_split();
    let mut lines = BufReader::new(rx).lines();
    match lines.next_line().await?.as_deref() {
        Some("pair") => {}
        other => bail!("unknown command {other:?}"),
    }
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (id, code) = auth.new_code(events_tx)?;
    info!("pairing code created, valid for {} s", CODE_TTL.as_secs());
    tx.write_all(format!("code {code} {}\n", CODE_TTL.as_secs()).as_bytes())
        .await?;

    let deadline = tokio::time::Instant::now() + CODE_TTL;
    let result = async {
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { return anyhow::Ok(()) };
                    tx.write_all(format!("{event}\n").as_bytes()).await?;
                    if !event.starts_with("wrong") {
                        return Ok(());
                    }
                }
                // The command was interrupted.
                _ = lines.next_line() => return Ok(()),
                _ = tokio::time::sleep_until(deadline) => {
                    tx.write_all(b"expired\n").await?;
                    return Ok(());
                }
            }
        }
    }
    .await;
    auth.cancel(id);
    result
}

/// `remcam-server pair`: ask the running server for a code, show it, and
/// report what happens until a client pairs or the code ends.
pub async fn run_pair_command(config_dir: &Path, server_name: &str) -> anyhow::Result<()> {
    let path = socket_path(config_dir)?;
    let stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "no running remcam-server uses {}; start the server first \
             (as the same user, with the same --config-dir)",
            config_dir.display()
        )
    })?;
    check_peer(&stream)?;
    let (rx, mut tx) = stream.into_split();
    tx.write_all(b"pair\n").await?;
    let mut lines = BufReader::new(rx).lines();
    while let Some(line) = lines.next_line().await? {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("code") => {
                let code = words.next().unwrap_or("?");
                let secs: u64 = words.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                println!(
                    "Pairing code: {code}   (one use, valid for {} minutes)",
                    secs / 60
                );
                println!();
                println!("On the client, run:");
                println!("    remcam-client {server_name} --pair {code}");
                println!();
                println!("Waiting for the client... (Ctrl-C cancels)");
            }
            Some("wrong") => {
                let left = words.next().unwrap_or("?");
                println!("A client tried a wrong code ({left} attempts left).");
            }
            Some("paired") => {
                let fp = words.next().unwrap_or("?");
                let name = words.next().unwrap_or("?");
                println!("Paired with {name}");
                println!("    fingerprint {fp}");
                return Ok(());
            }
            Some("failed") => {
                let reason = line.strip_prefix("failed").unwrap_or_default().trim();
                bail!("a client used the code, but the server couldn't save it: {reason}")
            }
            Some("expired") => bail!("the code expired; run `remcam-server pair` again"),
            Some("cancelled") => bail!("too many wrong attempts; the code was cancelled"),
            Some("replaced") => {
                bail!("the code was cancelled: another `remcam-server pair` started")
            }
            _ => bail!("unexpected reply from the server: {line}"),
        }
    }
    bail!("the server stopped")
}
