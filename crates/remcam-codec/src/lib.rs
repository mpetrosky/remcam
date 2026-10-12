//! Picture formats and codecs shared by the remcam server and client.
//!
//! - [`jpeg`]: camera MJPEG frames to planar YUV.
//! - [`h264`]: OpenH264 encoder (server) and decoder (client).
//! - [`Planes`]: the planar YUV picture passed between them, with
//!   conversions to YUYV (for v4l2loopback) and I420 (for the encoder).

pub mod h264;
pub mod jpeg;
mod yuv;

pub use yuv::Planes;
