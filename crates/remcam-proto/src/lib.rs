//! remcam wire protocol, v0.
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
pub const ALPN: &[u8] = b"remcam/0";
pub const PROTOCOL_VERSION: u32 = 0;
pub const DEFAULT_PORT: u16 = 7450;
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
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FormatInfo {
    pub fourcc: FourCC,
    /// Discrete (width, height) frame sizes the device offers in this format.
    pub sizes: Vec<(u32, u32)>,
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
    Close,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ServerMsg {
    Hello { version: u32, server_name: String },
    DeviceList { devices: Vec<DeviceInfo> },
    Opened { epoch: u32, format: VideoFormat },
    Error { msg: String },
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
}
