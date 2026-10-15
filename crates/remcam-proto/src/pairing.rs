//! Pairing: a short one-time code turns into mutual trust.
//!
//! The server shows a code; the person types it on the client. Both sides
//! run SPAKE2 with the code as the password over an already-encrypted (but
//! not yet authenticated) connection, then exchange confirmation MACs
//! keyed with the SPAKE2 result over:
//!
//! - TLS exporter output for this connection, which differs between the two
//!   halves of a machine-in-the-middle's connections, and
//! - both certificate fingerprints, which are what each side then stores.
//!
//! The code never crosses the network, and every guess needs a live
//! exchange with the server, which cancels the code after a few failures.
//!
//! ```text
//! → Pair        { client_name, spake }
//! ← Pair        { server_name, spake }
//! → PairConfirm { mac }          // server checks before trusting anything
//! ← PairConfirm { mac }          // client checks, then stores the server
//! ```

use anyhow::{Context, bail};
use ring::hmac;
use spake2::{Ed25519Group, Identity, Password, Spake2};

/// Crockford base32: no I, L, O or U, so codes are easy to read aloud.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// 8 characters, 40 bits.
const CODE_LEN: usize = 8;
const EXPORTER_LABEL: &[u8] = b"EXPORTER-remcam-pair";

/// A fresh random code, formatted for display as `XXXX-XXXX`.
pub fn generate_code() -> anyhow::Result<String> {
    let mut bytes = [0u8; 5];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .ok()
        .context("no system randomness")?;
    let bits = bytes.iter().fold(0u64, |acc, &b| acc << 8 | u64::from(b));
    let chars: String = (0..CODE_LEN)
        .rev()
        .map(|i| ALPHABET[(bits >> (i * 5)) as usize & 31] as char)
        .collect();
    Ok(format!("{}-{}", &chars[..4], &chars[4..]))
}

/// Canonical form of a typed code: case, dashes and spaces don't matter,
/// and O/I/L are read as 0/1/1.
pub fn normalize_code(code: &str) -> anyhow::Result<String> {
    let code: String = code
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect();
    if code.len() != CODE_LEN || !code.bytes().all(|b| ALPHABET.contains(&b)) {
        bail!("a pairing code is {CODE_LEN} letters and digits, like K7QM-4XRT");
    }
    Ok(code)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}

impl Role {
    fn label(self) -> &'static [u8] {
        match self {
            Role::Client => b"remcam pair client",
            Role::Server => b"remcam pair server",
        }
    }

    fn peer(self) -> Role {
        match self {
            Role::Client => Role::Server,
            Role::Server => Role::Client,
        }
    }
}

/// What ties the confirmation to this connection and these two identities.
pub fn binding(
    conn: &quinn::Connection,
    client_fp: &str,
    server_fp: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .ok()
        .context("TLS exporter unavailable")?;
    Ok(binding_from(&exporter, client_fp, server_fp))
}

fn binding_from(exporter: &[u8], client_fp: &str, server_fp: &str) -> Vec<u8> {
    let mut b = exporter.to_vec();
    b.extend_from_slice(client_fp.as_bytes());
    b.extend_from_slice(server_fp.as_bytes());
    b
}

/// One side of a pairing exchange, before the peer's SPAKE2 message.
pub struct Pake {
    role: Role,
    spake: Spake2<Ed25519Group>,
}

impl Pake {
    /// Start with an already [normalized](normalize_code) code. Returns the
    /// message to send to the peer.
    pub fn start(role: Role, code: &str) -> (Self, Vec<u8>) {
        let password = Password::new(code.as_bytes());
        let (a, b) = (
            Identity::new(Role::Client.label()),
            Identity::new(Role::Server.label()),
        );
        let (spake, msg) = match role {
            Role::Client => Spake2::<Ed25519Group>::start_a(&password, &a, &b),
            Role::Server => Spake2::<Ed25519Group>::start_b(&password, &a, &b),
        };
        (Self { role, spake }, msg)
    }

    /// Combine with the peer's message. A wrong code doesn't fail here, it
    /// gives a different key, so the MACs won't match.
    pub fn finish(self, peer_msg: &[u8], binding: Vec<u8>) -> anyhow::Result<Confirmation> {
        let key = self
            .spake
            .finish(peer_msg)
            .map_err(|e| anyhow::anyhow!("bad pairing message: {e:?}"))?;
        Ok(Confirmation {
            role: self.role,
            key: hmac::Key::new(hmac::HMAC_SHA256, &key),
            binding,
        })
    }
}

pub struct Confirmation {
    role: Role,
    key: hmac::Key,
    binding: Vec<u8>,
}

impl Confirmation {
    fn message(&self, role: Role) -> Vec<u8> {
        [role.label(), self.binding.as_slice()].concat()
    }

    /// Our MAC, to send.
    pub fn mac(&self) -> Vec<u8> {
        hmac::sign(&self.key, &self.message(self.role))
            .as_ref()
            .to_vec()
    }

    /// Whether the peer's MAC proves it used the same code on this connection.
    pub fn verify(&self, peer_mac: &[u8]) -> bool {
        hmac::verify(&self.key, &self.message(self.role.peer()), peer_mac).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(client_code: &str, server_code: &str, client_binding: &[u8]) -> (bool, bool) {
        let server_binding = binding_from(b"session", "cfp", "sfp");
        let (client, c_msg) = Pake::start(Role::Client, client_code);
        let (server, s_msg) = Pake::start(Role::Server, server_code);
        let client = client.finish(&s_msg, client_binding.to_vec()).unwrap();
        let server = server.finish(&c_msg, server_binding).unwrap();
        (server.verify(&client.mac()), client.verify(&server.mac()))
    }

    #[test]
    fn same_code_pairs() {
        let b = binding_from(b"session", "cfp", "sfp");
        assert_eq!(exchange("K7QM4XRT", "K7QM4XRT", &b), (true, true));
    }

    #[test]
    fn wrong_code_or_other_session_fails() {
        let b = binding_from(b"session", "cfp", "sfp");
        assert_eq!(exchange("K7QM4XRT", "K7QM4XRV", &b), (false, false));
        // Right code, but the client sees a different TLS session or
        // server certificate: a machine in the middle.
        let relayed = binding_from(b"other session", "cfp", "sfp");
        assert_eq!(exchange("K7QM4XRT", "K7QM4XRT", &relayed), (false, false));
        let other_server = binding_from(b"session", "cfp", "mitm");
        assert_eq!(
            exchange("K7QM4XRT", "K7QM4XRT", &other_server),
            (false, false)
        );
    }

    #[test]
    fn codes() {
        let code = generate_code().unwrap();
        assert_eq!(code.len(), 9);
        assert_eq!(code.as_bytes()[4], b'-');
        let normal = normalize_code(&code).unwrap();
        assert_eq!(normal, code.replace('-', ""));
        assert_eq!(normalize_code(&code.to_lowercase()).unwrap(), normal);
        assert_eq!(normalize_code("k7qm 4xrt").unwrap(), "K7QM4XRT");
        assert_eq!(normalize_code("OIL0-0000").unwrap(), "01100000");
        assert!(normalize_code("K7QM-4XR").is_err());
        assert!(normalize_code("K7QM-4XRU").is_err());
    }
}
