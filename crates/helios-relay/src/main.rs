use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use helios_relay::{DeviceStore, RelayConfig, RelayState, build_router, spawn_udp_relay};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "helios-relay",
    about = "Helios DigitalOcean Signaling & Integrated QUIC UDP Relay"
)]
struct Cli {
    /// Path to persistent TOML device allowlist file.
    #[arg(long, env = "HELIOS_STORE_PATH", default_value = "data/devices.toml")]
    store: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the WebSocket signaling server AND integrated QUIC UDP rendezvous/relay socket.
    Serve {
        /// Local HTTP/WS bind address (Caddy terminates TLS in front of this on DigitalOcean).
        #[arg(long, env = "HELIOS_BIND", default_value = "0.0.0.0:8080")]
        bind: SocketAddr,

        /// Local UDP bind address for NAT discovery & encrypted QUIC relay fallback.
        #[arg(long, env = "HELIOS_UDP_BIND", default_value = "0.0.0.0:3478")]
        udp_bind: SocketAddr,

        /// Public hostname or IP advertised to peers for UDP rendezvous/relay (e.g., relay.yourdomain.com).
        #[arg(long, env = "HELIOS_PUBLIC_HOST", default_value = "127.0.0.1")]
        public_host: String,

        /// Public UDP port advertised to peers (defaults to udp_bind's port).
        #[arg(long, env = "HELIOS_UDP_PORT", default_value_t = 3478)]
        udp_port: u16,

        /// Optional reusable bootstrap token for initial device enrollment.
        #[arg(long, env = "HELIOS_BOOTSTRAP_TOKEN")]
        bootstrap_token: Option<String>,

        /// Allow any valid Ed25519 key without prior pairing (DEVELOPMENT ONLY).
        #[arg(long, env = "HELIOS_OPEN_ENROLLMENT", default_value_t = false)]
        open_enrollment: bool,
    },

    /// Generate a one-time pairing token to authorize a new Windows Host or macOS Client.
    GenPairingToken {
        /// Token lifetime in seconds (default 15 minutes).
        #[arg(long, default_value_t = 900)]
        ttl_secs: u64,
    },

    /// List all paired/authorized devices.
    ListDevices,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve {
            bind,
            udp_bind,
            public_host,
            udp_port,
            bootstrap_token,
            open_enrollment,
        } => {
            let store = DeviceStore::load_or_create(&cli.store, bootstrap_token)?;
            let (bound_udp, _udp_handle) = spawn_udp_relay(udp_bind).await?;
            let advertised_port = if udp_port == 0 {
                bound_udp.port()
            } else {
                udp_port
            };
            let udp_relay_addr = format!("{public_host}:{advertised_port}");

            let config = RelayConfig {
                udp_relay_addr: udp_relay_addr.clone(),
                allow_open_enrollment: open_enrollment,
            };
            let state = RelayState::new(config, store);
            let app = build_router(state);

            info!(
                %bind,
                %bound_udp,
                %udp_relay_addr,
                store = %cli.store.display(),
                "starting helios-relay (WSS signaling + QUIC UDP rendezvous/relay)"
            );
            let listener = tokio::net::TcpListener::bind(bind).await?;
            axum::serve(listener, app).await?;
        }

        Command::GenPairingToken { ttl_secs } => {
            let mut store = DeviceStore::load_or_create(&cli.store, None)?;
            let token = store.issue_pairing_token(ttl_secs)?;
            println!("One-time pairing token (valid for {}s):", ttl_secs);
            println!("  {}", token.token);
        }

        Command::ListDevices => {
            let store = DeviceStore::load_or_create(&cli.store, None)?;
            let devices = store.list_devices();
            if devices.is_empty() {
                println!("No paired devices yet.");
            } else {
                for dev in devices {
                    println!(
                        "{} | {:<6} | {:<18} | {} | paired {}",
                        dev.device_id,
                        format!("{:?}", dev.role),
                        dev.name,
                        dev.os,
                        dev.paired_at
                    );
                }
            }
        }
    }

    Ok(())
}
