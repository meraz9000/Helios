//! Native Windows Service Control Manager (SCM) entrypoint (`HeliosHost`).
//!
//! When installed via `sc.exe create HeliosHost ... service --relay ...`,
//! Windows starts this binary at boot (before user login) and expects the
//! service dispatcher to report `StartPending -> Running -> StopPending -> Stopped`.

#[cfg(windows)]
pub mod scm {
    use std::ffi::OsString;
    use std::sync::OnceLock;
    use std::time::Duration;

    use anyhow::Result;
    use tokio::sync::oneshot;
    use tracing::{error, info};
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};

    use crate::service::{HostDaemonConfig, run_host_daemon};

    pub const SERVICE_NAME: &str = "HeliosHost";

    static SERVICE_CFG: OnceLock<HostDaemonConfig> = OnceLock::new();

    define_windows_service!(ffi_service_main, helios_service_main);

    pub fn run_as_windows_service(cfg: HostDaemonConfig) -> Result<()> {
        let _ = SERVICE_CFG.set(cfg);
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
        Ok(())
    }

    fn helios_service_main(_arguments: Vec<OsString>) {
        if let Err(err) = run_service_lifecycle() {
            error!("Helios Windows Service exited with error: {err:#}");
        }
    }

    fn run_service_lifecycle() -> Result<()> {
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop_tx = std::sync::Mutex::new(Some(stop_tx));

        let status_handle =
            service_control_handler::register(SERVICE_NAME, move |control_event| {
                match control_event {
                    ServiceControl::Stop | ServiceControl::Shutdown => {
                        if let Some(tx) = stop_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        ServiceControlHandlerResult::NoError
                    }
                    ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                    _ => ServiceControlHandlerResult::NotImplemented,
                }
            })?;

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        info!("Helios Windows Service reported Running to SCM");

        let cfg = SERVICE_CFG
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("service config not initialized"))?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        rt.block_on(async move {
            tokio::select! {
                res = run_host_daemon(cfg) => {
                    if let Err(err) = res {
                        error!("host daemon terminated: {err:#}");
                    }
                }
                _ = stop_rx => {
                    info!("received Windows SCM stop/shutdown signal");
                }
            }
        });

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        Ok(())
    }
}
