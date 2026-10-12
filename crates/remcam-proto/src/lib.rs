//! remcam wire protocol, v1.
//!
//! One QUIC connection per client. The client opens a single bidirectional
//! *control* stream and exchanges [`ClientMsg`] / [`ServerMsg`] on it. Each
//! video frame travels on its own server-opened unidirectional stream:
//! a length-prefixed [`FrameHeader`] followed by the raw payload until the
//! stream ends. A lost packet therefore only delays its own frame, and a
//! stale frame can be abandoned without affecting newer ones.

pub mod tls;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// QUIC ALPN identifier; bumped on incompatible wire changes.
pub const ALPN: &[u8] = b"remcam/1";
pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 7450;
/// DNS-SD service type servers advertise on the LAN. TXT records: `v` (protocol
/// version) and `fp` (certificate fingerprint).
pub const MDNS_SERVICE_TYPE: &str = "_remcam._udp.local.";
/// TLS server name used in certificates and when connecting.
pub const SERVER_NAME: &str = "remcam";

/// Upper bound for a single control message or frame header.
pub const MAX_MSG_LEN: u32 = 1 << 20;
/// Upper bound for a single frame payload (a 4K MJPEG frame is ~1–2 MB).
pub const MAX_FRAME_LEN: usize = 16 << 20;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCC(pub [u8; 4]);

impl FourCC {
    pub const MJPG: FourCC = FourCC(*b"MJPG");
}

impl std::fmt::Display for FourCC {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeviceInfo {
    /// Server-local identifier, currently the device path (e.g. `/dev/video4`).
    pub id: String,
    pub name: String,
    pub formats: Vec<FormatInfo>,
    /// Camera controls (exposure, focus, ...), with values as of listing time.
    pub controls: Vec<ControlInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FormatInfo {
    pub fourcc: FourCC,
    /// Discrete frame sizes the device offers in this format.
    pub sizes: Vec<FrameSize>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FrameSize {
    pub width: u32,
    pub height: u32,
    /// Frame rates offered at this size, highest first.
    pub fps: Vec<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ControlInfo {
    /// V4L2 control ID; what [`ClientMsg::SetControl`] refers to.
    pub id: u32,
    /// `v4l2-ctl`-style name, e.g. `exposure_time_absolute`.
    pub name: String,
    pub kind: ControlKind,
    pub default: i64,
    pub state: ControlState,
    pub read_only: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ControlKind {
    Integer {
        min: i64,
        max: i64,
        step: u64,
    },
    Boolean,
    /// `(value, label)` pairs; menus may have gaps in their values.
    Menu {
        items: Vec<(i64, String)>,
    },
}

/// The part of a control that changes at runtime.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlState {
    pub id: u32,
    pub value: i64,
    /// Set while another control overrides this one, e.g. exposure time
    /// while auto exposure is on. Setting it may fail or have no effect.
    pub inactive: bool,
}

impl ControlInfo {
    /// `value` as shown to people: menu labels, `true`/`false` for booleans.
    pub fn format_value(&self, value: i64) -> String {
        match &self.kind {
            ControlKind::Integer { .. } => value.to_string(),
            ControlKind::Boolean => (value != 0).to_string(),
            ControlKind::Menu { items } => items.iter().find(|(v, _)| *v == value).map_or_else(
                || value.to_string(),
                |(_, label)| format!("{value} ({label})"),
            ),
        }
    }

    /// One line: name, current value, default, accepted values, flags.
    pub fn describe(&self) -> String {
        let range = match &self.kind {
            ControlKind::Integer { min, max, step } if *step > 1 => {
                format!("{min}..={max} step {step}")
            }
            ControlKind::Integer { min, max, .. } => format!("{min}..={max}"),
            ControlKind::Boolean => "true|false".into(),
            ControlKind::Menu { items } => items
                .iter()
                .map(|(v, label)| format!("{v}={label}"))
                .collect::<Vec<_>>()
                .join(", "),
        };
        let flags = match (self.read_only, self.state.inactive) {
            (true, _) => "  [read-only]",
            (_, true) => "  [inactive]",
            _ => "",
        };
        format!(
            "{:<32} = {:<22} default {:<6} {range}{flags}",
            self.name,
            self.format_value(self.state.value),
            self.default
        )
    }
}

impl DeviceInfo {
    /// Multi-line listing of formats and controls.
    pub fn describe(&self) -> String {
        let mut out = format!("{}  {}\n", self.id, self.name);
        for f in &self.formats {
            for size in &f.sizes {
                let fps: Vec<_> = size.fps.iter().map(u32::to_string).collect();
                out += &format!(
                    "  {} {:>4}x{:<4}  fps {}\n",
                    f.fourcc,
                    size.width,
                    size.height,
                    fps.join(",")
                );
            }
        }
        for c in &self.controls {
            out += &format!("  {}\n", c.describe());
        }
        out
    }
}

/// Turn a V4L2 control name into the `v4l2-ctl` style:
/// `"Exposure Time, Absolute"` becomes `exposure_time_absolute`.
pub fn control_name(raw: &str) -> String {
    let mut name = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if !name.is_empty() && !name.ends_with('_') {
            name.push('_');
        }
    }
    name.trim_end_matches('_').to_string()
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFormat {
    pub fourcc: FourCC,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl std::fmt::Display for VideoFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}x{}@{}",
            self.fourcc, self.width, self.height, self.fps
        )
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ClientMsg {
    Hello {
        version: u32,
        client_name: String,
    },
    /// Start streaming `device_id` in (approximately) `format`. The server
    /// replies with [`ServerMsg::Opened`] carrying the format actually set.
    Open {
        device_id: String,
        format: VideoFormat,
    },
    /// Set a control on the open device. The server answers with
    /// [`ServerMsg::Controls`] on success, [`ServerMsg::Error`] otherwise.
    SetControl {
        id: u32,
        value: i64,
    },
    Close,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ServerMsg {
    Hello {
        version: u32,
        server_name: String,
    },
    DeviceList {
        devices: Vec<DeviceInfo>,
    },
    Opened {
        epoch: u32,
        format: VideoFormat,
    },
    /// Current state of every control on the open device. Sent after
    /// `Opened` and after each `SetControl`, since changing one control
    /// (e.g. auto exposure) can change others.
    Controls {
        controls: Vec<ControlState>,
    },
    Error {
        msg: String,
    },
}

/// Prefix of every video stream; the frame payload follows until stream end.
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct FrameHeader {
    /// Incremented on every `Open`, so frames from a previous session can be told apart.
    pub epoch: u32,
    pub seq: u64,
    /// Capture time in microseconds since the Unix epoch, on the server's clock.
    pub capture_ts_us: u64,
    pub len: u32,
}

/// Write `msg` as a big-endian u32 length followed by its postcard encoding.
pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = postcard::to_stdvec(msg)?;
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    Ok(())
}

/// Read one message written by [`write_msg`].
pub async fn read_msg<R, T>(r: &mut R) -> anyhow::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let len = r.read_u32().await.context("reading message length")?;
    if len > MAX_MSG_LEN {
        bail!("message length {len} exceeds limit");
    }
    let mut body = vec![0; len as usize];
    r.read_exact(&mut body)
        .await
        .context("reading message body")?;
    Ok(postcard::from_bytes(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip() {
        let msg = ClientMsg::Open {
            device_id: "/dev/video4".into(),
            format: VideoFormat {
                fourcc: FourCC::MJPG,
                width: 1280,
                height: 720,
                fps: 30,
            },
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &msg).await.unwrap();
        let back: ClientMsg = read_msg(&mut buf.as_slice()).await.unwrap();
        assert!(matches!(back, ClientMsg::Open { format, .. } if format.width == 1280));
    }

    #[test]
    fn control_names() {
        assert_eq!(
            control_name("Exposure Time, Absolute"),
            "exposure_time_absolute"
        );
        assert_eq!(
            control_name("White Balance Temperature, Auto"),
            "white_balance_temperature_auto"
        );
        assert_eq!(control_name("Power Line Frequency"), "power_line_frequency");
        assert_eq!(control_name("Pan (Absolute)"), "pan_absolute");
    }
}
