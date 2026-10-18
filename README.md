# remcam

Share a USB webcam over the network. See [DESIGN.md](DESIGN.md) for the
protocol and roadmap.

> **Status:** a personal project. It has only been tested on Linux, and it
> has not been security-audited. Treat the authentication as a good-faith
> design, not a guarantee, and prefer a VPN over exposing the port directly.

Current state: **M5**. The server captures MJPEG from a V4L2 camera and sends
it over QUIC, either as is or re-encoded to H.264 at a bitrate that adapts to
the link. The client decodes it into a v4l2loopback device, where any app
(OBS, browsers, Zoom, ffmpeg) sees it as a normal YUYV webcam. Servers are
found on the LAN via mDNS, camera controls (exposure, focus, white balance,
...) can be set from the client, and the client reconnects by itself if the
connection drops. Server and client authenticate each other: a client must
be paired with a server once, using a short one-time code.

## Workspace

| Crate | Role |
|---|---|
| `remcam-proto` | Wire messages, framing, QUIC/TLS config (self-signed certs on both sides), trust files, pairing |
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

# Once per client: pair. On the camera host, while the server runs:
./target/release/remcam-server pair            # shows a code like K7QM-4XRT
# On the other machine:
./target/release/remcam-client <host> --pair K7QM-4XRT

# Other machine, once per boot (needs v4l2loopback-dkms):
sudo modprobe v4l2loopback video_nr=10 card_label=remcam exclusive_caps=1

./target/release/remcam-client --discover      # servers on the LAN, paired or not
./target/release/remcam-client [<host>] --list
./target/release/remcam-client [<host>] \
    --device /dev/video0 --size 1920x1080 --fps 30 \
    --sink /dev/video10 --ctrl exposure_dynamic_framerate=false
```

`<host>` can be an address, a hostname, or the server's mDNS name. With no
host, the client uses the only server on the LAN.

`--size` defaults to 1280x720 (or the largest size offered), and `--fps` to
the highest rate at that size. Requests the camera doesn't offer are rejected
with the list of valid options. Other options: `--out DIR` saves frames
(`DIR/<seq>.jpg` for MJPEG, `DIR/stream.h264` for H.264, playable with
`ffplay`), `--frames N` stops after N frames, and `--stats SECS` sets the
statistics interval (0 turns it off).

### Pairing and trust

Both sides have a self-signed certificate, made on first run and identified
by its SHA-256 fingerprint, the same model as SSH keys. The connection is
always encrypted (QUIC uses TLS 1.3), and each side checks the other's
fingerprint:

| File (in `~/.config/remcam`) | Machine | What it is |
|---|---|---|
| `cert.der`, `key.der` | server | the server's identity |
| `client/cert.der`, `client/key.der` | client | the client's identity |
| `authorized_clients` | server | clients allowed in: `<fingerprint> <name>` per line |
| `known_servers` | client | servers trusted: `<name> <fingerprint>` per line |

Pairing fills both lists in one step. `remcam-server pair` asks the running
server for a code (single use, valid 5 minutes, cancelled after 3 wrong
tries) and waits until a client uses it. The client proves it knows the
code without sending it (SPAKE2), bound to that specific connection, so the
code can't be guessed offline or relayed by a machine in the middle.

Without pairing:

- **Server identity:** pass `--fingerprint <hex>` once; it's saved in
  `known_servers` when the connection works. A server already known under
  one name (say its address) is accepted under another (its hostname or mDNS
  name) and saved under that name too, as long as it's clear which server
  that is: the only one in `known_servers`, or the one whose fingerprint it
  advertises over mDNS.
- **Client identity:** a client the server doesn't know is turned away
  before it can list or open a camera, with an error showing its
  fingerprint. Adding that fingerprint to the server's `authorized_clients`
  by hand works as well as pairing.

`remcam-server clients` lists the authorized clients. Deleting a line from
`authorized_clients` locks that client out from its next connection, with no
restart needed. If a server's certificate changes (reinstalled, config
deleted), the client refuses to connect and names the `known_servers` line;
pair again to replace it. `--insecure` skips checking the server, but the
server still checks the client.

The server limits connections from one address to 30 a minute, and
connections that haven't said who they are yet to 32 at a time. While 4096
addresses have connected within the last minute, new addresses are refused.

### Across the internet

remcam authenticates every client, which is meant to make exposing it
reasonable, but it hasn't been audited, so a VPN is the safer choice. Two
easy options today:

- **Tailscale or WireGuard** on both machines (recommended): connect to the server's VPN
  address. Nothing to open on the router. This works well with remcam's
  UDP traffic; tunnelling through SSH or a TLS proxy doesn't, since those
  carry TCP and bring back the stalls and hidden congestion that QUIC avoids.
- **Forward UDP port 7450** on the camera host's router to the server, and
  connect to the public address. Only paired clients get past the handshake.

Direct connections through NAT without either (via `iroh`) are planned for M6.

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

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in this work shall be dual licensed as
above, without any additional terms or conditions.

### H.264 patents

The H.264 path uses [OpenH264](https://github.com/cisco/openh264), which is
built from bundled source and is itself BSD-2-Clause licensed. That license
does not cover H.264's patents. Cisco's patent coverage applies only to the
binaries it builds and distributes, not to a build from source. If you
distribute remcam binaries that include H.264 support, you may need a patent
license (for example from Via LA, formerly MPEG LA). The MJPEG path does not
involve H.264.
