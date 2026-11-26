# Helios

**Helios** is a low-bandwidth, **QUIC-powered** peer-to-peer remote desktop system built for accessing an always-on **Windows PC** from **macOS** — even over slow, jittery, or high-latency internet connections where TeamViewer and Splashtop stall or become unreadable.

## Key Capabilities

- **Always-On Windows Background Host (`helios-host`)**: Maintains a tiny outbound TLS heartbeat to your DigitalOcean droplet (~0% CPU/GPU when idle) and wakes its DXGI Desktop Duplication capture + QUIC worker only when an authenticated client connects.
- **Direct QUIC P2P First, Integrated Droplet UDP Relay Fallback (`helios-relay`)**: Uses a single \$4–\$6/month DigitalOcean droplet running one Rust binary (+ Caddy auto-TLS) for Ed25519-authenticated signaling, UDP NAT hole-punching, and transparent encrypted QUIC relay fallback when symmetric NAT or restrictive firewalls block direct P2P (no `coturn` required).
- **Built for Slow Internet**:
  - **Zero-Idle Capture**: Unchanged desktop frames transmit **0 kbps**.
  - **Survival / Text Mode**: 64x64 dirty-tile diffing with Zstd compression so terminals, IDEs, and text remain 100% razor-sharp even below 150 kbps.
  - **Native QUIC Prioritization & Zero Head-of-Line Blocking**: Mouse/keyboard input rides on a dedicated reliable bidirectional QUIC control stream, while small dirty-tile updates use **unreliable QUIC Datagrams (RFC 9221)** and larger frames use isolated per-frame unidirectional QUIC streams.
  - **Anti-Bufferbloat**: Strict 1-frame latest-wins slot prevents multi-second lag queues from ever building up, driven by native QUIC RTT and packet-loss telemetry.
- **Zero-Trust End-to-End Encryption**: Mutual Ed25519 device keypairs with one-time pairing tokens + **QUIC TLS 1.3 with pinned SHA-256 certificate fingerprints** so even the DigitalOcean relay cannot decrypt or MITM traffic.

## Repository Layout

```text
Helios/
├── crates/
│   ├── helios-proto/          # Wire protocol, Ed25519 auth/pairing, HLP1 UDP framing, Zstd dirty-tile codec
│   ├── helios-relay/          # DigitalOcean WSS signaling + integrated QUIC UDP NAT rendezvous & relay
│   ├── helios-transport/      # QUIC (quinn) P2P/Relay transport, pinned TLS 1.3 verifier, AdaptiveController
│   ├── helios-host-win/       # Windows always-on Host daemon (DXGI capture + Win32 SendInput + service install)
│   └── helios-client-core/    # Shared client engine, C-ABI FFI staticlib for macOS, and helios-cli diagnostic tool
├── apps/
│   └── helios-macos/          # Native SwiftUI + Metal + VideoToolbox macOS viewer app
├── deploy/
│   └── digitalocean/          # Docker Compose stack (helios-relay + Caddy auto-TLS) + droplet setup script
└── docs/
    └── DEPLOYMENT.md          # End-to-end setup guide for DigitalOcean, Windows PC, and macOS
```

## Quick Start (Local Development & Verification)

```bash
# Build and run all unit tests + direct P2P QUIC & Droplet UDP Relay Fallback integration tests
cargo test --workspace
```

See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for deploying the relay on your DigitalOcean droplet, installing the background Windows service, and connecting from macOS.
