mod capture;
mod clipboard;
mod encoder;
mod input;
mod installer;
mod service;
mod win_service;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use helios_proto::DeviceIdentity;
use service::{HostDaemonConfig, run_host_daemon};
use tracing_subscriber::EnvFilter;

pub const DEFAULT_RELAY_URL: &str = "wss://137-184-27-200.sslip.io/ws";

fn default_identity_path() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(installer::win::DEFAULT_WIN_IDENTITY)
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("host-identity.json")
    }
}

fn default_host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Windows-PC".into())
}

#[derive(Debug, Parser)]
#[command(
    name = "helios-host",
    about = "Helios Always-On Windows Remote Desktop Host (double-click to install & start)"
)]
struct Cli {
    /// Path to persisted Ed25519 device identity & unique Access Passcode JSON.
    #[arg(long, env = "HELIOS_IDENTITY_PATH")]
    identity: Option<PathBuf>,

    /// Friendly display name for this Host PC (defaults to your PC's hostname).
    #[arg(long, env = "HELIOS_HOST_NAME")]
    name: Option<String>,

    /// Omit subcommand on Windows (double-click!) to auto-install & start in the background.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install Helios Host to Program Files as an always-on hidden background task and start it now.
    Install {
        #[arg(long, env = "HELIOS_RELAY_URL", default_value = DEFAULT_RELAY_URL)]
        relay: String,
    },

    /// Uninstall the always-on background task and stop Helios Host.
    Uninstall,

    /// Run the host daemon (pass --background on Windows to detach from console window).
    Run {
        #[arg(long, env = "HELIOS_RELAY_URL", default_value = DEFAULT_RELAY_URL)]
        relay: String,

        /// Optional one-time relay pairing token (not required when relay is in lab/open mode).
        #[arg(long, env = "HELIOS_PAIRING_TOKEN")]
        pairing_token: Option<String>,

        /// Monitor index to capture (default 0 = primary monitor).
        #[arg(long, default_value_t = 0)]
        monitor: u32,

        /// Hide console window on Windows (used automatically by the background task).
        #[arg(long, default_value_t = false)]
        background: bool,

        /// Force synthetic test screen capture (useful for headless testing).
        #[arg(long, default_value_t = false)]
        synthetic: bool,
    },

    /// Run under Windows Service Control Manager.
    Service {
        #[arg(long, env = "HELIOS_RELAY_URL", default_value = DEFAULT_RELAY_URL)]
        relay: String,

        #[arg(long, default_value_t = 0)]
        monitor: u32,
    },

    /// Print (or initialize) this PC's Helios Host ID and unique Access Passcode.
    Identity,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let identity_path = cli.identity.unwrap_or_else(default_identity_path);
    let host_name = cli.name.unwrap_or_else(default_host_name);

    // Default behavior when double-clicked (no arguments):
    // On Windows -> seamless one-click install + start hidden background host + show Access Passcode popup!
    let command = cli.command.unwrap_or_else(|| {
        #[cfg(windows)]
        {
            Command::Install {
                relay: DEFAULT_RELAY_URL.to_string(),
            }
        }
        #[cfg(not(windows))]
        {
            Command::Run {
                relay: DEFAULT_RELAY_URL.to_string(),
                pairing_token: None,
                monitor: 0,
                background: false,
                synthetic: false,
            }
        }
    });

    match command {
        Command::Install { relay } => {
            #[cfg(windows)]
            {
                if let Err(err) = installer::win::install_and_start(&relay, &identity_path, &host_name) {
                    installer::win::show_error_box(
                        "Helios Host Install Error",
                        &format!("Could not install Helios Host:\n\n{err:#}"),
                    );
                    return Err(err);
                }
            }
            #[cfg(not(windows))]
            {
                let id = DeviceIdentity::load_or_create(&identity_path, &host_name)?;
                installer::write_access_info_file(&identity_path, &id, &relay);
                println!("Host ID: {} | Access Passcode: {}", id.device_id, id.access_passcode);
            }
        }

        Command::Uninstall => {
            #[cfg(windows)]
            {
                installer::win::uninstall()?;
            }
            #[cfg(not(windows))]
            {
                println!("Uninstall is only applicable on Windows.");
            }
        }

        Command::Run {
            relay,
            pairing_token,
            monitor,
            background,
            synthetic,
        } => {
            #[cfg(windows)]
            if background {
                installer::win::hide_console_window();
            }
            #[cfg(not(windows))]
            let _ = background;

            let identity = DeviceIdentity::load_or_create(&identity_path, &host_name)?;
            installer::write_access_info_file(&identity_path, &identity, &relay);
            println!("==========================================");
            println!("  Helios Host Name : {}", identity.name);
            println!("  Helios Host ID   : {}", identity.device_id);
            println!("  Access Passcode  : {}", identity.access_passcode);
            println!("==========================================");

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
            let identity = DeviceIdentity::load_or_create(&identity_path, &host_name)?;
            installer::write_access_info_file(&identity_path, &identity, &relay);
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
            let identity = DeviceIdentity::load_or_create(&identity_path, &host_name)?;
            println!("Host Name       : {}", identity.name);
            println!("Device ID       : {}", identity.device_id);
            println!("Access Passcode : {}", identity.access_passcode);
            println!("Key File        : {}", identity_path.display());
        }
    }

    Ok(())
}
