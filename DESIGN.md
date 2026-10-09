# remcam — network webcam protocol (design investigation)

Goal: make a USB webcam on machine A appear as an ordinary local camera
(`/dev/videoN`) on machine B, first on a LAN, later across the internet.

## 1. What already exists (and why it isn't enough)

The premise "no protocol exists" is only partly true. Existing options:

| Approach | What it does | Why it falls short |
|---|---|---|
| **USB/IP** (`usbip`, in mainline kernel) | Forwards raw USB URBs, so the remote UVC driver sees the real device | Webcams use isochronous transfers with tight timing; uncompressed USB traffic; very fragile over Wi-Fi and unusable over WAN; one client at a time |
| **RDP camera redirection** (MS-RDPECAM, supported by recent FreeRDP 3.x clients) | Redirects a camera into a Windows RDP session | Only works inside an RDP session, client→server direction only |
| **RTSP / RTP / SRT / WebRTC / NDI** | General video transport | Carry video, but the receiver doesn't get a *camera*: no device node, no format negotiation, no camera controls (focus, exposure, zoom), no on-demand start |
| **ffmpeg/GStreamer → v4l2loopback** | Ad-hoc pipeline | Works, but it's a hand-assembled shell pipeline: no discovery, no auth, no controls, no reconnection |
| **PipeWire** | Local video graph | Its network modules (RTP, pulse-tunnel) are audio-only |

The real gap is a **camera-aware** protocol: one that knows about devices,
formats, and controls, and has a receiver that shows up as a real camera.

### "Lack of speed" is a USB-level problem, not a network problem

| Stream @ 1080p30 | Bitrate | Fits |
|---|---|---|
| Raw YUYV 4:2:2 (what USB/IP-style forwarding implies) | ~995 Mbit/s | Barely fits gigabit |
| MJPEG passthrough (camera's own compression) | ~20–60 Mbit/s | Wired LAN, good Wi-Fi |
| H.264 / AV1 re-encode | ~2–6 Mbit/s | WAN |

**Conclusion: forward at the V4L2 frame level, not at the USB level.** The
camera already compresses for us.

## 2. Architecture

```
 machine A (camera host)                          machine B (consumer)
┌──────────────────────────┐   QUIC (TLS 1.3)  ┌────────────────────────────┐
│ remcam-server            │ ────────────────▶ │ remcam-client              │
│  V4L2 capture (mmap)     │  control: bidi    │  decode MJPEG → YUYV/I420  │
│  MJPEG passthrough       │  video:   1 uni   │  write to v4l2loopback     │
│  [later: H.264 encoder]  │  stream per frame │  /dev/videoN ← Zoom, OBS,  │
│  ctrl get/set            │                   │                browser...  │
└──────────────────────────┘                   └────────────────────────────┘
        ▲ mDNS advertise  _remcam._udp.local
```

### Transport: QUIC (`quinn`)

QUIC fits both phases without a later rewrite:

- **TLS 1.3 built in**, so it's WAN-ready from day one.
- **One unidirectional stream per frame.** A lost packet only stalls its own
  frame. If a frame is too old (newer one already done), the receiver
  `stop_sending`s it and the sender `reset`s it. That's a simple "drop stale
  frames" policy with no head-of-line blocking across frames.
- **A long-lived bidirectional control stream** for negotiation and camera
  controls, with reliable, ordered delivery.
- Congestion control and RTT stats (`Connection::stats()`) are free inputs to
  adaptive bitrate later.
- QUIC DATAGRAMs (RFC 9221) are an option for tiny frames, but per-frame
  streams are simpler and handle MJPEG frames of 100–300 KB without our own
  fragmentation.

Rejected alternatives: raw UDP/RTP (need our own fragmentation, encryption, and
congestion control); TCP (head-of-line blocking causes latency spikes on
loss); WebRTC (huge surface; consider later only for browser viewers; `str0m`
is the leanest Rust option).

### Receiver sink: v4l2loopback (Phase 1), PipeWire (later)

- `v4l2loopback` (Debian: `v4l2loopback-dkms`, needs root once) gives every
  app a normal `/dev/videoN`. It's the most compatible option.
- Many apps don't accept MJPEG from a loopback device, so the client decodes
  to YUYV or I420 (`zune-jpeg` is pure Rust; `turbojpeg` is faster).
- Later: a PipeWire video source node (`pipewire` crate) for portal-based
  apps (Flatpak browsers), with no kernel module needed.

## 3. Wire protocol (v0 sketch)

ALPN `remcam/0`. Messages are `serde` + `postcard` with a u32 length prefix.

Control stream (client ↔ server):
```
Hello        { version, client_name }
DeviceList   { devices: [ {id, name, formats: [{fourcc, w, h, fps[]}], controls: [...] } ] }
Open         { device_id, fourcc, w, h, fps, codec: Passthrough | H264 { bitrate } }
Opened       { stream_epoch }
SetControl   { id, value }   /  ControlChanged { id, value }
RequestKeyframe               // H.264 only
Bitrate      { target_kbps }  // client- or server-driven ABR
Close / Error { code, msg }
```

Video (server → client), one uni stream per frame:
```
FrameHeader { epoch, seq: u64, capture_ts_us: u64, keyframe: bool, len: u32 }
<payload bytes>
```
`capture_ts_us` comes from the V4L2 buffer timestamp (CLOCK_MONOTONIC). It's
used for latency measurement and, later, A/V sync if a mic is forwarded.

## 4. Crates

| Purpose | Crate |
|---|---|
| V4L2 capture (server) and output (client loopback) | `v4l` (libv4l-rs) |
| QUIC / TLS / self-signed certs | `quinn`, `rustls`, `rcgen` |
| Async runtime | `tokio` |
| Serialization | `serde`, `postcard` |
| LAN discovery | `mdns-sd` |
| JPEG decode | `zune-jpeg` or `turbojpeg` |
| H.264 (Phase 2) | `openh264` (software), or VAAPI through `ffmpeg-next` / GStreamer (`gstreamer` crate) |
| WAN NAT traversal (Phase 3) | `iroh` (QUIC + hole punching + relay fallback) |

## 5. Roadmap

0. **Validate the path without Rust** (needs `v4l2loopback`):
   ```sh
   # B: sudo modprobe v4l2loopback video_nr=10 card_label=remcam exclusive_caps=1
   # B: ffmpeg -i udp://0.0.0.0:5000 -f v4l2 -pix_fmt yuyv422 /dev/video10
   # A: ffmpeg -f v4l2 -input_format mjpeg -video_size 1280x720 -framerate 30 \
   #       -i /dev/video4 -c copy -f mjpeg udp://B:5000?pkt_size=1316
   ```
   This confirms that loopback-as-a-camera works with your target apps. Expect
   it to break on packet loss; that's what the real protocol fixes.
1. **M1 (done):** server captures MJPEG with `v4l` and sends frames over quinn. Client
   writes `.jpg` files and logs latency and frame rate.
2. **M2 (done):** client decodes and writes to v4l2loopback. End-to-end camera in
   OBS or a browser. Decoding uses libjpeg-turbo straight to planar YUV, then
   interleaves to YUYV with a full-to-limited range LUT. `zune-jpeg` (RGB only,
   so JPEG→RGB→YUYV) is the fallback without the `turbojpeg` feature, and for
   chroma layouts other than 4:2:2/4:2:0. Note that the `turbojpeg` crate 1.x
   requires libjpeg-turbo ≥ 3.0, but Debian trixie ships 2.1.5, so we pin 0.5.x.
3. **M3:** device and format negotiation, camera control forwarding, mDNS
   discovery, reconnection, and stale-frame dropping.
4. **M4:** H.264 encode, keyframe requests, and adaptive bitrate from QUIC
   stats. Wi-Fi becomes reliable.
5. **M5 (WAN):** auth with an SSH-style trust-on-first-use cert fingerprint or
   a pairing code, NAT traversal (`iroh`, or "use Tailscale/WireGuard" as the
   zero-effort answer), and a relay fallback.

## 6. Open questions

- **On-demand capture:** start the camera only when an app opens the loopback
  device. v4l2loopback offers no clean open/close event, so we'd need to poll
  the device state. Needs experimentation.
- **Multiple clients per camera:** fan out on the server (easy with
  passthrough, costlier with per-client encoders).
- **Audio:** many webcams include a mic. It could be forwarded on the same
  connection as Opus, or left to PipeWire's existing RTP module.
- **Windows/macOS receivers:** the protocol is portable, but the sink isn't.
  See section 7.

## 7. Platform notes (not a current goal)

### Virtual camera sink

| OS | Replacement for v4l2loopback | Notes |
|---|---|---|
| Linux | v4l2loopback (now), PipeWire node (later) | |
| Windows 11 | Media Foundation virtual camera (`MFCreateVirtualCamera`) | Needs a COM media source; Windows 10 needs a DirectShow filter instead |
| macOS 12.3+ | CoreMediaIO Camera Extension | Ships inside a signed app bundle, and the user must approve it |

All of these expect NV12 or YUY2 frames, so "decode MJPEG to YUV" carries over
unchanged. Only the hand-off to the OS differs.

### JPEG decoding

| OS | libjpeg-turbo (portable, SIMD) | Native / hardware alternative |
|---|---|---|
| Linux | `libturbojpeg0-dev` (apt) | VAAPI JPEG decode (Intel/AMD GPUs) |
| macOS | `brew install jpeg-turbo` | VideoToolbox (hardware JPEG, outputs YUV directly) |
| Windows | vcpkg `libjpeg-turbo`, or the official installer | Media Foundation MJPEG decoder MFT; D3D11 hardware JPEG decode on Intel GPUs |

- The `turbojpeg` crate either builds its bundled libjpeg-turbo (cmake, plus
  `nasm` on x86 only; ARM uses NEON and needs neither) or links a system copy
  (`pkg-config` feature, or `TURBOJPEG_LIB_DIR` / `TURBOJPEG_INCLUDE_DIR`). The
  bundled build links statically, so it's a build-time dependency, not a
  runtime one.
- `nasm` is available via `brew install nasm` on macOS and
  `winget install NASM.NASM` on Windows.
- `zune-jpeg` (pure Rust) builds everywhere with no setup and remains the
  fallback.
- The native decoders avoid the CPU cost entirely, but each needs its own
  platform-specific code path.
