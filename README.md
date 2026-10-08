# remcam

Share a USB webcam over the network. See [DESIGN.md](DESIGN.md) for the
protocol and roadmap.

Current state: **M1**. The server captures MJPEG from a V4L2 camera and sends
it over QUIC. The client receives it, logs throughput and latency, and can
save frames as JPEG files. It does not appear as a local camera yet (that's M2).

## Workspace

| Crate | Role |
|---|---|
| `remcam-proto` | Wire messages, framing, QUIC/TLS config (fingerprint-pinned self-signed certs) |
| `remcam-server` | Runs on the machine with the camera |
| `remcam-client` | Runs on the machine that wants the camera |

## Usage

```sh
cargo build --release

# Camera host. The certificate is created in ~/.config/remcam on first run,
# and its fingerprint is printed at startup.
./target/release/remcam-server --list          # show shareable cameras
./target/release/remcam-server                 # listens on udp/0.0.0.0:7450

# Other machine:
./target/release/remcam-client <host> --fingerprint <hex> \
    --device /dev/video4 --size 1920x1080 --fps 30 \
    [--out frames/] [--frames 300]
```

`--insecure` skips the fingerprint check. The connection is still encrypted,
but the server isn't authenticated. Set `RUST_LOG=debug` to see dropped and
stale frame details.

The latency figure compares the server's capture timestamp with the client's
wall clock, so across machines it's only meaningful when both are NTP-synced.
