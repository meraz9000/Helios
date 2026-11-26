#!/usr/bin/env bash
set -euo pipefail

# Bootstrap script for a fresh Ubuntu DigitalOcean Droplet ($4-$6/mo is plenty).
# Run as root or with sudo from deploy/digitalocean/.

if [[ ! -f .env ]]; then
  echo "Generating .env from .env.example..."
  cp .env.example .env
  echo "ACTION REQUIRED: Edit .env to set HELIOS_PUBLIC_HOST to your domain, then re-run this script."
  exit 0
fi

echo "Configuring UFW firewall rules for Helios WSS signaling + QUIC UDP rendezvous/relay..."
if command -v ufw >/dev/null 2>&1; then
  ufw allow 22/tcp comment "SSH"
  ufw allow 80/tcp comment "Caddy ACME HTTP"
  ufw allow 443/tcp comment "Helios WSS Signaling"
  ufw allow 443/udp comment "Caddy HTTP/3"
  ufw allow 3478/udp comment "Helios QUIC UDP Rendezvous & Relay"
  ufw --force enable
fi

echo "Launching Helios QUIC Relay + Caddy auto-TLS..."
docker compose up -d --build

echo ""
echo "Helios DigitalOcean Relay is running!"
echo "Generate a one-time pairing token for your Windows PC or Mac with:"
echo "  docker exec -it helios-relay helios-relay gen-pairing-token"
