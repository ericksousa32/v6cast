# v6cast

Low-latency direct IPv6 screen sharing, written in Rust, for Linux and Windows.

v6cast sends the stream straight from one machine to another over IPv6, without a
relay server, a NAT traversal service or an account.

## Status

v6cast is being built in four stages (see the technical plan in the issue tracker):

| Stage | Scope | Status |
|-------|-------|--------|
| 1 | Network engine: wire format, QUIC over IPv6, group-token auth, IPv6 discovery, telemetry | Code done; the real Fedora–Windows test is not done yet |
| 2 | Media pipeline: capture, scaling, H.264 encode/decode, local loopback | Not implemented |
| 3 | Runtime: connects the network engine to the media pipeline | Not implemented |
| 4 | Graphical interface | Not implemented |

Today the repository contains the network engine and empty skeletons for the other crates.
There is no working screen-sharing application yet.

## Supported platforms

- Fedora Linux
- Windows (CI builds and tests it; no test on real Windows hardware has been run yet)

## Architecture

The code is a Cargo workspace with five crates:

| Crate | Role | State |
|-------|------|-------|
| `v6cast-protocol` | Binary wire format: media datagram header, control messages, frame fragmentation and reassembly. No I/O, no async. | Implemented |
| `v6cast-net` | Network engine: QUIC (`quinn`) server and client over IPv6-only sockets, multi-viewer fan-out, IPv6 discovery, telemetry | Implemented |
| `v6cast-media` | Screen/window capture, scaling, H.264 codec, local rendering | Skeleton (Stage 2) |
| `v6cast-runtime` | Ties the network and media crates together, session management | Skeleton (Stage 3) |
| `v6cast-ui` | Graphical interface | Skeleton (Stage 4) |

### Transport

- **QUIC over UDP/IPv6.** Sockets are bound IPv6-only, so IPv4-mapped peers cannot reach
  the server.
- **Media uses QUIC datagrams** (unreliable). A late fragment is dropped rather than
  delaying the queue. Each datagram stays within `IPV6_SAFE_DATAGRAM` (1150 bytes), and
  frames are split across datagrams with a 28-byte media header.
- **Control uses one reliable QUIC stream.** It carries the handshake, preset and frame-rate
  negotiation, keyframe requests and telemetry reports.
- **Fan-out:** one encoded stream is copied to every viewer. A slow viewer skips frames
  instead of holding up the others.

### Authentication

- Each server instance generates a self-signed TLS 1.3 certificate at startup.
- Peers authenticate with a shared **group token**. The client sends it in the first
  control message (`Hello`). The server compares it in constant time (`subtle`) and either
  answers or closes the connection at once with `AUTH_FAILED`.
- A client that sends nothing is closed after the authentication timeout (5 seconds by
  default).

Read the [known limits](#known-limits) before relying on this: the client does not pin
the server's certificate.

### IPv6 discovery

The endpoint a viewer connects to is the stable global unicast address (GUA) of the
active interface. Selection rules:

- Link-local (`fe80::/10`), unique local (`fc00::/7`), loopback and multicast addresses
  are ignored.
- Stable addresses win over temporary (RFC 4941 privacy) addresses, and current addresses
  win over deprecated ones. A temporary address is used only when no stable one exists.
- `EndpointMonitor` polls the system every **5 seconds** and publishes the new endpoint
  when the address changes.

### Telemetry

Receivers measure RTT, packet loss, jitter and bandwidth, and report them on the control
stream.

## Build and test

### Install Rust

Install the stable toolchain with [rustup](https://rustup.rs). The minimal profile does not
include the formatter or the linter, so add them:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup component add rustfmt clippy
```

The network engine uses `ring`, which compiles C and assembly code. A C compiler
(for example `gcc` on Fedora or the MSVC build tools on Windows) must be available.

### Build, test and lint

Run these from the repository root:

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

### What CI checks

The workflow in `.github/workflows/ci.yml` runs on pushes to `main`, `dev` and `feat/**`,
and on pull requests to `main` and `dev`. It runs on `ubuntu-latest` and `windows-latest`
and executes, in order:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`

## Isolated build environment

`dev/Containerfile` builds a Fedora 45 image with the Rust toolchain, the media
libraries (PipeWire, VA-API, ALSA, Vulkan) and the VA-API H.264 driver from RPM Fusion.
Nothing is installed on the host. The image is built with rootless podman.

Build the image (`dev/run.sh` also builds it on first use):

```bash
podman build -t localhost/v6cast-dev:latest -f dev/Containerfile dev
```

Run a command in it:

```bash
dev/run.sh cargo test --workspace
dev/run.sh cargo clippy --workspace --all-targets -- -D warnings
dev/run.sh vainfo
```

`dev/run.sh` mounts the repository at `/work`, passes `/dev/dri` through, and keeps the
cargo registry and build output in named volumes (`v6cast-cargo-home`, `v6cast-target`).
The host's `target/` is not used. It runs without `--privileged` and without sudo.

What the container covers:

- building, unit tests and integration tests;
- VA-API H.264 through `/dev/dri` (`vainfo` lists the H.264 encode entrypoints on an AMD GPU).

What it does not cover:

- PipeWire and XDG portal screen capture need the host's Wayland session. Run the
  compiled binary on the host for manual capture tests.

## Network setup

v6cast does not pick a port for you. The server is started with a bind address such as
`[::]:PORT`, where `PORT` is a UDP port you choose. Use the same port in the firewall rules
below. The commands are examples; check them against your own setup before running them.

### Fedora (firewalld)

Open the UDP port in the zone that your network uses:

```bash
sudo firewall-cmd --get-default-zone
sudo firewall-cmd --permanent --add-port=PORT/udp
sudo firewall-cmd --reload
sudo firewall-cmd --list-ports
```

firewalld applies the rule to IPv4 and IPv6 traffic in the zone, so no IPv6-specific
option is needed. Check that the zone is the one your active connection uses
(`firewall-cmd --get-active-zones`).

### Windows Defender Firewall

Run PowerShell as administrator. Allow the v6cast program, or the UDP port:

```powershell
New-NetFirewallRule -DisplayName "v6cast (UDP PORT)" -Direction Inbound `
  -Protocol UDP -LocalPort PORT -Profile Private -Action Allow

# Or allow one program (the v6cast.exe path is an example; the binary is not built yet):
New-NetFirewallRule -DisplayName "v6cast" -Direction Inbound `
  -Program "C:\path\to\v6cast.exe" -Protocol UDP -Profile Private -Action Allow
```

Use the `Private` profile for home networks. Only use `Public` if you understand the risk.

### Router (IPv6 inbound)

Viewers reach the host only if the router forwards inbound UDP to the host's global IPv6
address. Many routers block inbound IPv6 by default. In the router's firewall settings:

1. Find the host's global address, which `v6cast` publishes (or `ip -6 addr` on Linux,
   `ipconfig` on Windows, look for the address that is not `fe80::` or `fd…`/`fc…`).
2. Add an inbound rule that allows UDP `PORT` to that address.

The menu names depend on the router, so follow its own documentation.

## Known limits

- **No certificate pinning yet.** The client accepts any server certificate and checks
  only the handshake signature, so an active man-in-the-middle could relay the connection
  and learn the group token. Pinning is planned for a later stage. Use v6cast only on
  networks you trust until then.
- **Discovery polls every 5 seconds.** A change of address is published up to 5 seconds
  late.
- **No overlay fallback.** If IPv6 is not reachable end to end, v6cast does not connect.
  There is no relay or alternative transport.
- **No application yet.** Stages 2 to 4 (capture, encoding, runtime, interface) are not
  implemented. The Windows firewall example refers to a binary that does not exist yet.

## Development

The workspace uses Conventional Commits. Work happens on `feat/*` branches, with pull
requests into `dev`.

## Licence

See [LICENSE](LICENSE).
