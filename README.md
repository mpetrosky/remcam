# remcam

Share a USB webcam over the network. See [DESIGN.md](DESIGN.md) for the
protocol and roadmap.

Current state: **M4**. The server captures MJPEG from a V4L2 camera and sends
it over QUIC, either as is or re-encoded to H.264 at a bitrate that adapts to
the link. The client decodes it into a v4l2loopback device, where any app
(OBS, browsers, Zoom, ffmpeg) sees it as a normal YUYV webcam. Servers are
found on the LAN via mDNS, camera controls (exposure, focus, white balance,
...) can be set from the client, and the client reconnects by itself if the
connection drops.

## Workspace

| Crate | Role |
|---|---|
| `remcam-proto` | Wire messages, framing, QUIC/TLS config (fingerprint-pinned self-signed certs) |
| `remcam-codec` | JPEG decoding, YUV conversions, H.264 encode/decode (OpenH264) |
| `remcam-server` | Runs on the machine with the camera |
| `remcam-client` | Runs on the machine that wants the camera |

## Usage

```sh
cargo build --release

# Camera host. The certificate is created in ~/.config/remcam on first run,
# and its fingerprint is printed at startup. The server advertises itself
# on the LAN via mDNS (disable with --no-mdns).
./target/release/remcam-server --list          # cameras, sizes/frame rates, controls
./target/release/remcam-server                 # listens on udp/0.0.0.0:7450

# Other machine, once per boot (needs v4l2loopback-dkms):
sudo modprobe v4l2loopback video_nr=10 card_label=remcam exclusive_caps=1

./target/release/remcam-client --discover      # servers on the LAN, with fingerprints
./target/release/remcam-client [<host>] --fingerprint <hex> --list
./target/release/remcam-client [<host>] --fingerprint <hex> \
    --device /dev/video0 --size 1920x1080 --fps 30 \
    --sink /dev/video10 --ctrl exposure_dynamic_framerate=false
```

`<host>` can be an address, a hostname, or the server's mDNS name. With no
host, the client uses the only server on the LAN. A fingerprint is still
required: mDNS is unauthenticated, so the client shows the advertised
fingerprint but won't trust it on its own. Compare it with the server's
startup output.

`--size` defaults to 1280x720 (or the largest size offered), and `--fps` to
the highest rate at that size. Requests the camera doesn't offer are rejected
with the list of valid options. Other options: `--out DIR` saves frames
(`DIR/<seq>.jpg` for MJPEG, `DIR/stream.h264` for H.264, playable with
`ffplay`), `--frames N` stops after N frames, and `--stats SECS` sets the
statistics interval (0 turns it off).

### H.264 or MJPEG

`--codec h264` (the default) has the server decode the camera's JPEG frames
and re-encode them with OpenH264. `--codec mjpeg` passes the camera's frames
through untouched. Measured on one i5-8250U with server and client on the
same machine, with a deliberately noisy picture (dark room, brightness and
gamma at maximum), which is a hard case for any encoder:

| 30 fps | Bitrate | Latency |
|---|---|---|
| MJPEG 720p | 110 Mbit/s | 36 ms |
| H.264 720p | 2.8 Mbit/s (default cap) | 50 ms |
| MJPEG 1080p | 178 Mbit/s | 37 ms |
| H.264 1080p | 6.2 Mbit/s (default cap) | 60 ms |

MJPEG costs the server almost nothing. H.264 at 1080p costs it about 26 ms of
CPU per frame on this machine (JPEG decode 13 ms, conversion 2.5 ms, encode
10.5 ms spread over several threads), so slower machines may need 720p.

A clean, well-lit picture needs far less bitrate for MJPEG (typically
20–60 Mbit/s), so MJPEG is the better choice on wired LAN or good Wi-Fi,
when latency or picture quality matter most. H.264 is the right choice for
anything slower or shared.

`--bitrate KBPS` caps the H.264 bitrate. The default cap is 0.1 bits per
pixel (2765 kbit/s for 720p30). Below the cap, the server adapts to the link:
it backs off when frames pile up unacknowledged, when QUIC reports loss, or
when the round-trip time rises; it creeps back up when the link is clear. It
logs each step down. Through a test relay that throttled the link from 4 to
1.5 to 0.8 Mbit/s and back, it settled within about 5 s of each change with
latency peaking around 350 ms, kept 30 fps, and returned to the cap within
about 25 s of the link recovering.

H.264 frames depend on earlier ones, so the client decodes them strictly in
order. If it ever has to skip one (it fell too far behind), it asks the
server for a keyframe and shows the last good picture until then. The
stats line counts keyframes.

OpenH264 is built from source (bundled with the `openh264` crate; needs a C++
compiler). At startup of each H.264 stream it prints a warning that "bitrate
can't be controlled ... without enabling skip frame"; that is expected, since
remcam drops frames itself before encoding and the bitrate does track the
target. Note that Cisco's H.264 patent license covers only its prebuilt
OpenH264 binaries, not builds from source.

### Camera controls

Controls use `v4l2-ctl`-style names (see `--list`). Set them at startup with
`--ctrl NAME=VALUE` (repeatable). When the client runs in a terminal, you can
also type commands while it streams:

```
c                              list controls and current values
brightness=20                  set a control (or: set brightness 20)
set auto_exposure manual mode  menus take a number or label
white_balance_automatic=off    switches take true/false/on/off
reset brightness               back to the default
q                              quit
```

Changes the camera makes as a side effect are reported too. For example,
switching `auto_exposure` to manual makes `exposure_time_absolute` active.

Many webcams lower their frame rate in dim light to expose longer
(`exposure_dynamic_framerate`, or `exposure_auto_priority` on older kernels).
Turning that off took one test camera from 10 fps at ~100 ms latency to
30 fps at ~35 ms.

### Reconnection

Once streaming has started, a dropped connection makes the client retry with
backoff (1 s, doubling up to 10 s) until it gets back in or you quit. The
loopback device stays open meanwhile, so apps using it keep the camera. They
see a frozen frame until the stream resumes. `--ctrl` settings are reapplied
on every reconnect. A server that disappears without closing the connection
is noticed within about 6–8 s. Errors before the first successful stream
(bad fingerprint, unknown camera, unsupported size) end the client instead,
since retrying wouldn't fix them.

### v4l2loopback

`exclusive_caps=1` is required for Chrome and other WebRTC apps to list the
device as a camera. To load the module at boot, put `v4l2loopback` in
`/etc/modules-load.d/remcam.conf` and
`options v4l2loopback video_nr=10 card_label=remcam exclusive_caps=1` in
`/etc/modprobe.d/remcam.conf`.

JPEG frames (MJPEG on the client, and the camera's frames on the server for
H.264) are decoded with libjpeg-turbo by default (Debian: `libturbojpeg0-dev`),
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

To measure the H.264 path on your own machine, record some camera frames and
run the benchmark:

```sh
ffmpeg -f v4l2 -input_format mjpeg -video_size 1280x720 -i /dev/video0 \
    -frames:v 90 -c copy frames/%03d.jpg
cargo run --release -p remcam-codec --example h264_bench -- frames [KBPS] [FPS]
```
