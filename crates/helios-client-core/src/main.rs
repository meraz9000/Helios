use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use clap::{Parser, Subcommand};
use helios_client_core::{ClientSession, ClientSessionConfig};
use helios_proto::{DeviceId, DeviceIdentity, QualityMode};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "helios-cli",
    about = "Helios Client Diagnostic CLI (pair, list hosts, verify QUIC P2P/Relay stream)"
)]
struct Cli {
    /// Path to persisted Ed25519 client identity JSON.
    #[arg(long, env = "HELIOS_IDENTITY_PATH", default_value = "client-identity.json")]
    identity: PathBuf,

    /// Friendly display name for this macOS/Client machine.
    #[arg(long, env = "HELIOS_CLIENT_NAME", default_value = "MacBook")]
    name: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print (or initialize) this Client's Helios Device ID and Ed25519 public key.
    Identity,

    /// List all online Windows Hosts registered on the DigitalOcean relay.
    ListHosts {
        #[arg(long, env = "HELIOS_RELAY_URL")]
        relay: String,

        #[arg(long, env = "HELIOS_PAIRING_TOKEN")]
        pairing_token: Option<String>,
    },

    /// Connect to a target Windows Host over QUIC and print live FPS, kbps, and RTT stats.
    Connect {
        #[arg(long, env = "HELIOS_RELAY_URL")]
        relay: String,

        /// Target Windows Host Device ID (e.g., HL-A1B2-C3D4).
        #[arg(long)]
        host_id: String,

        #[arg(long, env = "HELIOS_PAIRING_TOKEN")]
        pairing_token: Option<String>,

        /// Initial quality mode: auto | survival | low | balanced | smooth
        #[arg(long, default_value = "auto")]
        mode: String,

        /// Force DigitalOcean UDP relay fallback instead of direct P2P (for testing).
        #[arg(long, default_value_t = false)]
        force_relay: bool,
    },
}

fn parse_mode(s: &str) -> QualityMode {
    match s.to_ascii_lowercase().as_str() {
        "survival" | "text" => QualityMode::SurvivalText,
        "low" | "slow" => QualityMode::LowBandwidth,
        "balanced" => QualityMode::Balanced,
        "smooth" => QualityMode::Smooth,
        _ => QualityMode::Auto,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let identity = DeviceIdentity::load_or_create(&cli.identity, &cli.name)?;

    match cli.command {
        Command::Identity => {
            println!("Client Name: {}", identity.name);
            println!("Device ID  : {}", identity.device_id);
            println!("Public Key : {}", identity.public_key_b64());
        }

        Command::ListHosts {
            relay,
            pairing_token,
        } => {
            let cfg = ClientSessionConfig {
                relay_url: relay,
                identity,
                pairing_token,
                initial_mode: QualityMode::Auto,
                force_relay: false,
            };
            let hosts = ClientSession::list_online_hosts(&cfg).await?;
            if hosts.is_empty() {
                println!("No online Hosts currently connected to relay.");
            } else {
                for h in hosts {
                    println!(
                        "{} | {:<18} | {:<8} | active_sessions={}",
                        h.device_id, h.name, h.os, h.active_sessions
                    );
                }
            }
        }

        Command::Connect {
            relay,
            host_id,
            pairing_token,
            mode,
            force_relay,
        } => {
            let cfg = ClientSessionConfig {
                relay_url: relay,
                identity,
                pairing_token,
                initial_mode: parse_mode(&mode),
                force_relay,
            };
            let frames = Arc::new(AtomicU64::new(0));
            let frames_cb = frames.clone();

            let (_session, handle) = ClientSession::connect_and_stream(
                cfg,
                DeviceId(host_id),
                Box::new(move |w, h, _rgba, stats| {
                    let count = frames_cb.fetch_add(1, Ordering::Relaxed) + 1;
                    if count == 1 || count.is_multiple_of(15) {
                        let path = if stats.using_relay { "RELAY" } else { "P2P" };
                        println!(
                            "[frame #{count} {path}] {w}x{h} | RTT={:.1}ms | {:.1} FPS | {} kbps",
                            stats.rtt_ms, stats.fps, stats.video_kbps
                        );
                    }
                }),
            )
            .await?;

            handle.await??;
        }
    }

    Ok(())
}
