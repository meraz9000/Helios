# Helios Deployment Guide

## 1. Deploy the Relay on Your DigitalOcean Droplet

A \$4–\$6/month Ubuntu droplet runs both the WSS signaling server and the integrated QUIC UDP NAT rendezvous / fallback relay inside a single lightweight container (behind Caddy for automatic HTTPS/WSS).

1. Point a DNS `A` record (e.g., `relay.yourdomain.com`) to your droplet's public IPv4 address.
2. SSH into your droplet, install Docker + Docker Compose, and clone `Helios`:
   ```bash
   git clone https://github.com/meraz9000/Helios.git
   cd Helios/deploy/digitalocean
   chmod +x setup-droplet.sh
   sudo ./setup-droplet.sh
   ```
3. Edit `deploy/digitalocean/.env` to set `HELIOS_PUBLIC_HOST=relay.yourdomain.com`, then re-run:
   ```bash
   sudo ./setup-droplet.sh
   ```
4. Verify health over HTTPS:
   ```bash
   curl https://relay.yourdomain.com/health
   ```
5. Generate two one-time pairing tokens (one for your Windows PC, one for your Mac):
   ```bash
   docker exec -it helios-relay helios-relay gen-pairing-token
   docker exec -it helios-relay helios-relay gen-pairing-token
   ```

---

## 2. Install the Always-On Windows Host

Build on Windows (with Rust installed):

```powershell
cargo build --release -p helios-host-win
```

### First-Time Pairing Test (Foreground)

```powershell
.\target\release\helios-host.exe `
  --identity "C:\ProgramData\Helios\host-identity.json" `
  --name "My-Windows-PC" `
  run `
  --relay wss://relay.yourdomain.com/ws `
  --pairing-token PAIR-XXXX-XXXX-XXXX-XXXX
```

Note the printed `HL-XXXX-XXXX` Device ID. Once paired, the `--pairing-token` flag is no longer needed.

### Install as an Auto-Starting Background Windows Service

Print the exact `sc.exe` commands for your binary path:

```powershell
.\target\release\helios-host.exe print-service-install --relay wss://relay.yourdomain.com/ws
```

---

## 3. Connect from macOS

### Diagnostic CLI (`helios-cli`)

Build and pair your Mac:

```bash
cargo build --release -p helios-client-core

# Pair and list online Windows Hosts
./target/release/helios-cli list-hosts \
  --relay wss://relay.yourdomain.com/ws \
  --pairing-token PAIR-YYYY-YYYY-YYYY-YYYY

# Connect over QUIC in Survival/Text Mode and observe live RTT, FPS, and kbps
./target/release/helios-cli connect \
  --relay wss://relay.yourdomain.com/ws \
  --host-id HL-XXXX-XXXX \
  --mode survival
```

### Native macOS SwiftUI + Metal + VideoToolbox App (`apps/helios-macos`)

```bash
./apps/helios-macos/build_macos.sh
```
