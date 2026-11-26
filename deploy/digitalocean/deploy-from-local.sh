#!/usr/bin/env bash
set -euo pipefail

# Run this script from your local machine (Mac/PC with SSH access to your DigitalOcean droplet).
DROPLET_USER="${DROPLET_USER:-root}"
DROPLET_IP="${DROPLET_IP:-137.184.27.200}"
HELIOS_DOMAIN="${HELIOS_DOMAIN:-137-184-27-200.sslip.io}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

echo "==> 1/3 Syncing Helios relay source to ${DROPLET_USER}@${DROPLET_IP}:/opt/helios..."
ssh "${DROPLET_USER}@${DROPLET_IP}" "mkdir -p /opt/helios"
rsync -az --delete \
  --exclude '.git' \
  --exclude 'target' \
  --exclude 'deploy/digitalocean/data' \
  --exclude 'deploy/digitalocean/.env' \
  "${REPO_ROOT}/Cargo.toml" \
  "${REPO_ROOT}/Cargo.lock" \
  "${REPO_ROOT}/crates" \
  "${REPO_ROOT}/deploy" \
  "${DROPLET_USER}@${DROPLET_IP}:/opt/helios/"

echo "==> 2/3 Installing Docker (if missing), opening firewall, and launching Helios Relay + Caddy TLS..."
ssh "${DROPLET_USER}@${DROPLET_IP}" "HELIOS_DOMAIN='${HELIOS_DOMAIN}' bash -s" <<'REMOTE'
set -euo pipefail

if ! command -v docker >/dev/null 2>&1; then
  echo "Installing Docker..."
  curl -fsSL https://get.docker.com | sh
  systemctl enable --now docker
fi

cd /opt/helios/deploy/digitalocean

cat > .env <<EOF
HELIOS_PUBLIC_HOST=${HELIOS_DOMAIN}
HELIOS_UDP_PORT=3478
HELIOS_BOOTSTRAP_TOKEN=
RUST_LOG=info
EOF

if command -v ufw >/dev/null 2>&1; then
  ufw allow 22/tcp comment "SSH"
  ufw allow 80/tcp comment "Caddy ACME HTTP"
  ufw allow 443/tcp comment "Helios WSS Signaling"
  ufw allow 443/udp comment "Caddy HTTP/3"
  ufw allow 3478/udp comment "Helios QUIC UDP Rendezvous & Relay"
  ufw --force enable
fi

docker compose up -d --build

echo "Waiting for helios-relay healthcheck..."
for i in $(seq 1 30); do
  if curl -fsS "https://${HELIOS_DOMAIN}/health" >/dev/null 2>&1; then
    echo ""
    echo "Relay health response:"
    curl -fsS "https://${HELIOS_DOMAIN}/health"
    echo ""
    break
  fi
  sleep 2
done

echo ""
echo "=========================================================="
echo "  Helios QUIC Relay is LIVE!"
echo "  WSS URL: wss://${HELIOS_DOMAIN}/ws"
echo "  UDP NAT/Relay: ${HELIOS_DOMAIN}:3478/udp"
echo "=========================================================="
echo ""
echo "One-time pairing token #1 (for your Windows Host PC):"
docker exec helios-relay helios-relay gen-pairing-token --ttl-secs 3600
echo ""
echo "One-time pairing token #2 (for your macOS Viewer):"
docker exec helios-relay helios-relay gen-pairing-token --ttl-secs 3600
REMOTE

echo "==> 3/3 Done!"
