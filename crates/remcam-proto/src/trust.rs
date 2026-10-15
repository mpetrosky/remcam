//! The trust files, kept in the config directory:
//!
//! - `authorized_clients` (server): `<fingerprint> <name>` per line, the
//!   clients allowed to connect, like SSH's `authorized_keys`.
//! - `known_servers` (client): `<name> <fingerprint>` per line, the servers
//!   this client trusts, like SSH's `known_hosts`.
//!
//! `#` starts a comment. Malformed lines are skipped with a warning, so one
//! bad edit doesn't lock everyone out.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use tracing::warn;

use crate::tls::parse_fingerprint;

pub const AUTHORIZED_CLIENTS: &str = "authorized_clients";
pub const KNOWN_SERVERS: &str = "known_servers";

/// `$XDG_CONFIG_HOME/remcam`, or `~/.config/remcam`.
pub fn default_config_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(dir).join("remcam"));
    }
    let home = std::env::var_os("HOME").context("neither XDG_CONFIG_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".config/remcam"))
}

/// Make a name safe to store on one line as a single field.
pub fn sanitize_name(name: &str) -> String {
    let name: String = name
        .trim()
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-:[]".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        "unnamed".into()
    } else {
        name
    }
}

/// One parsed line: a name and a fingerprint, plus its line number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub fingerprint: String,
    pub line: usize,
}

/// Read `path`, a missing file being empty. `fp_first` gives the column order.
fn read_entries(path: &Path, fp_first: bool) -> anyhow::Result<Vec<Entry>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut entries = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(a), Some(b), None) = (fields.next(), fields.next(), fields.next()) else {
            warn!(
                "{}:{line_no}: expected two fields, skipping",
                path.display()
            );
            continue;
        };
        let (fp, name) = if fp_first { (a, b) } else { (b, a) };
        match parse_fingerprint(fp) {
            Ok(fingerprint) => entries.push(Entry {
                name: name.to_string(),
                fingerprint,
                line: line_no,
            }),
            Err(e) => warn!("{}:{line_no}: {e:#}, skipping", path.display()),
        }
    }
    Ok(entries)
}

/// Append one line, creating the file (owner-only) with `header` if needed.
fn append_line(path: &Path, header: &str, line: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut text = String::new();
    if f.metadata()?.len() == 0 {
        text += header;
    }
    text += line;
    text.push('\n');
    f.write_all(text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// The server's list of allowed clients. Read afresh on every lookup, so
/// edits take effect for the next connection without a restart.
#[derive(Debug, Clone)]
pub struct AuthorizedClients {
    path: PathBuf,
}

impl AuthorizedClients {
    pub fn new(config_dir: &Path) -> Self {
        Self {
            path: config_dir.join(AUTHORIZED_CLIENTS),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> anyhow::Result<Vec<Entry>> {
        read_entries(&self.path, true)
    }

    /// The name the client was authorized under, if it is.
    pub fn lookup(&self, fingerprint: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .entries()?
            .into_iter()
            .find(|e| e.fingerprint == fingerprint)
            .map(|e| e.name))
    }

    pub fn add(&self, fingerprint: &str, name: &str) -> anyhow::Result<()> {
        if self.lookup(fingerprint)?.is_some() {
            return Ok(());
        }
        append_line(
            &self.path,
            "# Clients allowed to use this remcam server: <fingerprint> <name>.\n\
             # Delete a line to revoke that client (takes effect on its next connection).\n",
            &format!("{fingerprint} {}", sanitize_name(name)),
        )
    }
}

/// The client's list of trusted servers, keyed by the name used to reach
/// them (host, host:port, or mDNS name).
#[derive(Debug, Clone)]
pub struct KnownServers {
    path: PathBuf,
    entries: Vec<Entry>,
}

impl KnownServers {
    pub fn load(config_dir: &Path) -> anyhow::Result<Self> {
        let path = config_dir.join(KNOWN_SERVERS);
        let entries = read_entries(&path, false)?;
        Ok(Self { path, entries })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Names are compared case-insensitively, after [`sanitize_name`].
    pub fn key(name: &str) -> String {
        sanitize_name(name).to_ascii_lowercase()
    }

    pub fn lookup(&self, name: &str) -> Option<&Entry> {
        let key = Self::key(name);
        self.entries
            .iter()
            .find(|e| e.name.to_ascii_lowercase() == key)
    }

    /// Every trusted fingerprint, under any name.
    pub fn fingerprints(&self) -> Vec<String> {
        let mut fps: Vec<String> = self.entries.iter().map(|e| e.fingerprint.clone()).collect();
        fps.sort();
        fps.dedup();
        fps
    }

    pub fn contains_fingerprint(&self, fingerprint: &str) -> bool {
        self.entries.iter().any(|e| e.fingerprint == fingerprint)
    }

    /// Remember `name` as `fingerprint`. Does nothing if that's already
    /// known; a different fingerprint for the same name is an error.
    pub fn add(&mut self, name: &str, fingerprint: &str) -> anyhow::Result<()> {
        if let Some(e) = self.lookup(name) {
            if e.fingerprint == fingerprint {
                return Ok(());
            }
            anyhow::bail!(
                "{}:{} already has a different fingerprint for {name}",
                self.path.display(),
                e.line
            );
        }
        let key = Self::key(name);
        append_line(
            &self.path,
            "# Servers this remcam client trusts: <name> <fingerprint>.\n",
            &format!("{key} {fingerprint}"),
        )?;
        // Re-read so line numbers stay right for later messages.
        self.entries = read_entries(&self.path, false)?;
        Ok(())
    }

    /// Remember `name` as `fingerprint`, dropping any other fingerprint
    /// stored for it. Only for identities proven by pairing.
    pub fn replace(&mut self, name: &str, fingerprint: &str) -> anyhow::Result<()> {
        if let Some(old) = self.lookup(name).cloned() {
            if old.fingerprint == fingerprint {
                return Ok(());
            }
            let text = std::fs::read_to_string(&self.path)
                .with_context(|| format!("reading {}", self.path.display()))?;
            let kept: String = text
                .lines()
                .enumerate()
                .filter(|(i, _)| *i + 1 != old.line)
                .map(|(_, l)| format!("{l}\n"))
                .collect();
            std::fs::write(&self.path, kept)
                .with_context(|| format!("writing {}", self.path.display()))?;
            self.entries = read_entries(&self.path, false)?;
        }
        self.add(name, fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("remcam-trust-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    const FP_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FP_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn authorized_clients() {
        let dir = temp_dir("auth");
        let clients = AuthorizedClients::new(&dir);
        assert_eq!(clients.lookup(FP_A).unwrap(), None);
        clients.add(FP_A, "my laptop").unwrap();
        clients.add(FP_A, "again").unwrap();
        assert_eq!(clients.lookup(FP_A).unwrap().as_deref(), Some("my_laptop"));
        assert_eq!(clients.entries().unwrap().len(), 1);

        // Hand edits: comments, junk, uppercase hex with colons.
        let mut text = std::fs::read_to_string(clients.path()).unwrap();
        let upper: Vec<String> = FP_B
            .to_uppercase()
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();
        text += &format!(
            "garbage line here\n{} phone # added by hand\n",
            upper.join(":")
        );
        std::fs::write(clients.path(), text).unwrap();
        assert_eq!(clients.lookup(FP_B).unwrap().as_deref(), Some("phone"));

        let mode = std::fs::metadata(clients.path()).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn known_servers() {
        let dir = temp_dir("known");
        let mut known = KnownServers::load(&dir).unwrap();
        assert!(known.lookup("camhost").is_none());
        known.add("CamHost", FP_A).unwrap();
        known.add("camhost", FP_A).unwrap();
        assert!(known.add("camhost", FP_B).is_err());
        known.add("192.168.1.5:7450", FP_A).unwrap();

        let known = KnownServers::load(&dir).unwrap();
        assert_eq!(known.lookup("CAMHOST").unwrap().fingerprint, FP_A);
        assert_eq!(known.lookup("192.168.1.5:7450").unwrap().line, 3);
        assert_eq!(known.fingerprints(), vec![FP_A.to_string()]);
        assert!(!known.contains_fingerprint(FP_B));

        let mut known = known;
        known.replace("camhost", FP_B).unwrap();
        let known = KnownServers::load(&dir).unwrap();
        assert_eq!(known.lookup("camhost").unwrap().fingerprint, FP_B);
        assert_eq!(known.lookup("192.168.1.5:7450").unwrap().fingerprint, FP_A);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
