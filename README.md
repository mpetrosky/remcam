# remcam

Share a USB webcam over the network. See [DESIGN.md](DESIGN.md) for the
protocol and roadmap.

Current state: **M2**. The server captures MJPEG from a V4L2 camera and sends
it over QUIC. The client decodes it into a v4l2loopback device, where any app
(OBS, browsers, Zoom, ffmpeg) sees it as a normal YUYV webcam.

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

# Other machine, once per boot (needs v4l2loopback-dkms):
sudo modprobe v4l2loopback video_nr=10 card_label=remcam exclusive_caps=1

./target/release/remcam-client <host> --fingerprint <hex> \
    --device /dev/video4 --size 1920x1080 --fps 30 \
    --sink /dev/video10 [--out frames/] [--frames 300]
```

`exclusive_caps=1` is required for Chrome and other WebRTC apps to list the
device as a camera. To load the module at boot, put `v4l2loopback` in
`/etc/modules-load.d/remcam.conf` and
`options v4l2loopback video_nr=10 card_label=remcam exclusive_caps=1` in
`/etc/modprobe.d/remcam.conf`.

The client decodes with libjpeg-turbo by default (Debian: `libturbojpeg0-dev`),
straight to YUV. Build with `--no-default-features` to use the pure-Rust
`zune-jpeg` decoder instead, with no system dependency. At 1080p on an
i5-8250U (powersave governor):

| Decoder | ms per frame, CPU boosted / live pacing | Client CPU at ~23 fps |
|---|---|---|
| libjpeg-turbo (default) | 9 / 18 | ~53% of one core |
| zune-jpeg | 16 / 29 | ~65% of one core |

The time that matters is the live-pacing one, because the CPU doesn't boost
between frames. At 30 fps the budget is 33 ms per frame. If the decoder falls
behind, the newest frame wins and the stats line reports `sink dropped`.
