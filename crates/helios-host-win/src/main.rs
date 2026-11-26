mod capture;
mod clipboard;
mod encoder;
mod input;
mod service;
mod win_service;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use helios_proto::DeviceIdentity;
use service::{HostDaemonConfig, run_host_daemon};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "helios-host",
    about = "Helios Always-On Windows Remote Desktop Host"
)]
struct Cli {
    /// Path to persisted Ed25519 device identity JSON.
    #[arg(long, env = "HELIOS_IDENTITY_PATH", default_value = "host-identity.json")]
    identity: PathBuf,

    /// Friendly display name for this Host PC.
    #[arg(long, env = "HELIOS_HOST_NAME", default_value = "Windows-PC")]
    name: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the host daemon in the foreground (for first-time pairing & debugging).
    Run {
        /// DigitalOcean relay WebSocket URL (e.g., wss://relay.yourdomain.com/ws).
        #[arg(long, env = "HELIOS_RELAY_URL")]
        relay: String,

        /// One-time pairing token from `helios-relay gen-pairing-token` (first run only).
        #[arg(long, env = "HELIOS_PAIRING_TOKEN")]
        pairing_token: Option<String>,

        /// Monitor index to capture (default 0 = primary monitor).
        #[arg(long, default_value_t = 0)]
        monitor: u32,

        /// Force synthetic test screen capture (useful for headless testing).
        #[arg(long, default_value_t = false)]
        synthetic: bool,
    },

    /// Run under Windows Service Control Manager (invoked automatically by SCM at boot).
    Service {
        /// DigitalOcean relay WebSocket URL (e.g., wss://relay.yourdomain.com/ws).
        #[arg(long, env = "HELIOS_RELAY_URL")]
        relay: String,

        /// Monitor index to capture (default 0 = primary monitor).
        #[arg(long, default_value_t = 0)]
        monitor: u32,
    },

    /// Print (or initialize) this PC's Helios Device ID and Ed25519 public key.
    Identity,

    /// Print the Windows `sc.exe` commands to install Helios as an always-on boot service.
    PrintServiceInstall {
        /// DigitalOcean relay WebSocket URL.
        #[arg(long)]
        relay: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let identity = DeviceIdentity::load_or_create(&cli.identity, &cli.name)?;

    match cli.command {
        Command::Run {
            relay,
            pairing_token,
            monitor,
            synthetic,
        } => {
            let cfg = HostDaemonConfig {
                relay_url: relay,
                identity,
                pairing_token,
                monitor_index: monitor,
                force_synthetic_capture: synthetic,
            };
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_host_daemon(cfg))?;
        }

        Command::Service { relay, monitor } => {
            let cfg = HostDaemonConfig {
                relay_url: relay,
                identity,
                pairing_token: None,
                monitor_index: monitor,
                force_synthetic_capture: false,
            };
            #[cfg(windows)]
            {
                win_service::scm::run_as_windows_service(cfg)?;
            }
            #[cfg(not(windows))]
            {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(run_host_daemon(cfg))?;
            }
        }

        Command::Identity => {
            println!("Host Name : {}", identity.name);
            println!("Device ID : {}", identity.device_id);
            println!("Public Key: {}", identity.public_key_b64());
            println!("Key File  : {}", cli.identity.display());
        }

        Command::PrintServiceInstall { relay } => {
            let exe = std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| r"C:\Program Files\Helios\helios-host.exe".into());
            println!("Run the following in an Administrator PowerShell on Windows:");
            println!();
            println!(
                "sc.exe create HeliosHost binPath= \"\\\"{exe}\\\" --identity \\\"C:\\ProgramData\\Helios\\host-identity.json\\\" service --relay {relay}\" start= auto DisplayName= \"Helios Remote Desktop Host\""
            );
            println!("sc.exe failure HeliosHost reset= 86400 actions= restart/5000/restart/10000/restart/30000");
            println!("sc.exe start HeliosHost");
        }
    }

    Ok(())
}
