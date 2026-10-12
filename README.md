# remcam

Share a USB webcam over the network. See [DESIGN.md](DESIGN.md) for the
protocol and roadmap.

Current state: **M3**. The server captures MJPEG from a V4L2 camera and sends
it over QUIC. The client decodes it into a v4l2loopback device, where any app
(OBS, browsers, Zoom, ffmpeg) sees it as a normal YUYV webcam. Servers are
found on the LAN via mDNS, camera controls (exposure, focus, white balance,
...) can be set from the client, and the client reconnects by itself if the
connection drops.

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
with the list of valid options. Other options: `--out DIR` saves frames as
JPEG, `--frames N` stops after N frames, and `--stats SECS` sets the
statistics interval (0 turns it off).

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
