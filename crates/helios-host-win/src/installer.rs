//! Zero-CLI Windows double-click installer & hidden background runner.
//!
//! Double-clicking `helios-host.exe` on Windows:
//! 1. Requests UAC elevation once (`ShellExecuteW("runas")`) if not already elevated.
//! 2. Copies itself to `C:\Program Files\Helios\helios-host.exe`.
//! 3. Generates/loads the persistent Ed25519 identity + unique Access Passcode at
//!    `C:\ProgramData\Helios\host-identity.json` (and writes `ACCESS_INFO.txt`).
//! 4. Registers an always-on, auto-restarting elevated Task Scheduler task (`HeliosHost`)
//!    running in the interactive desktop session (so DXGI GPU capture & Hardware H.264
//!    work natively without Session 0 black-screen limits!) and starts it immediately
//!    with zero visible console window.
//! 5. Displays a native Windows popup with your Host Name, Host ID, and unique Access Passcode.

use std::path::Path;

use helios_proto::DeviceIdentity;

/// Write a human-readable copy of the Host ID and Access Passcode next to the identity file.
pub fn write_access_info_file(identity_path: &Path, identity: &DeviceIdentity, relay_url: &str) {
    let info_path = identity_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("ACCESS_INFO.txt"))
        .unwrap_or_else(|| std::path::PathBuf::from("ACCESS_INFO.txt"));

    let contents = format!(
        "Helios Remote Desktop Host — Connection Details\r\n\
         ===============================================\r\n\
         Host Name       : {}\r\n\
         Host ID         : {}\r\n\
         Access Passcode : {}\r\n\
         Relay URL       : {}\r\n\r\n\
         Open HeliosViewer on your Mac, select this PC, and enter the Access Passcode above.\r\n",
        identity.name, identity.device_id, identity.access_passcode, relay_url
    );
    let _ = std::fs::write(info_path, contents);
}

#[cfg(windows)]
pub mod win {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Context, Result};
    use helios_proto::DeviceIdentity;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Console::FreeConsole;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW, SW_SHOWNORMAL,
    };
    use windows::core::PCWSTR;

    use crate::clipboard::set_host_clipboard_text;
    use super::write_access_info_file;

    pub const INSTALL_DIR: &str = r"C:\Program Files\Helios";
    pub const INSTALL_EXE: &str = r"C:\Program Files\Helios\helios-host.exe";
    pub const DATA_DIR: &str = r"C:\ProgramData\Helios";
    pub const DEFAULT_WIN_IDENTITY: &str = r"C:\ProgramData\Helios\host-identity.json";
    pub const TASK_NAME: &str = "HeliosHost";

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn hide_console_window() {
        unsafe {
            let _ = FreeConsole();
        }
    }

    pub fn show_info_box(title: &str, body: &str) {
        let w_title = to_wide(title);
        let w_body = to_wide(body);
        unsafe {
            MessageBoxW(
                HWND::default(),
                PCWSTR(w_body.as_ptr()),
                PCWSTR(w_title.as_ptr()),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }

    pub fn show_error_box(title: &str, body: &str) {
        let w_title = to_wide(title);
        let w_body = to_wide(body);
        unsafe {
            MessageBoxW(
                HWND::default(),
                PCWSTR(w_body.as_ptr()),
                PCWSTR(w_title.as_ptr()),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    fn is_elevated() -> bool {
        // Probing `net session` is a zero-dependency check for Windows Administrator token
        Command::new("net")
            .arg("session")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Re-launch current exe via Windows UAC prompt (`runas`) with `args`.
    fn relaunch_elevated(args: &str) -> bool {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let op = to_wide("runas");
        let file = to_wide(&exe.to_string_lossy());
        let params = to_wide(args);
        let hinst = unsafe {
            ShellExecuteW(
                HWND::default(),
                PCWSTR(op.as_ptr()),
                PCWSTR(file.as_ptr()),
                PCWSTR(params.as_ptr()),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        (hinst.0 as isize) > 32
    }

    /// One-click install & start as an always-on invisible background process.
    pub fn install_and_start(relay_url: &str, identity_path: &Path, host_name: &str) -> Result<()> {
        if !is_elevated() {
            let args = format!(
                "--identity \"{}\" --name \"{}\" install --relay \"{}\"",
                identity_path.display(),
                host_name,
                relay_url
            );
            if relaunch_elevated(&args) {
                return Ok(());
            }
            // User declined UAC — continue running directly in background for current session
            show_error_box(
                "Helios Host",
                "Administrator permission was declined. Please re-run and approve the UAC prompt to install Helios as an always-on auto-starting background host.",
            );
            return Ok(());
        }

        // Hide console during GUI install flow
        hide_console_window();

        std::fs::create_dir_all(INSTALL_DIR).context("failed to create C:\\Program Files\\Helios")?;
        std::fs::create_dir_all(DATA_DIR).context("failed to create C:\\ProgramData\\Helios")?;

        let identity = DeviceIdentity::load_or_create(identity_path, host_name)?;
        write_access_info_file(identity_path, &identity, relay_url);

        // Stop any existing HeliosHost task / process before replacing the binary
        let _ = Command::new("schtasks")
            .args(["/End", "/TN", TASK_NAME])
            .output();
        let current_pid = std::process::id();
        let ps_kill = format!(
            "Get-Process helios-host -ErrorAction SilentlyContinue | Where-Object {{ $_.Id -ne {current_pid} }} | Stop-Process -Force -ErrorAction SilentlyContinue"
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps_kill])
            .output();
        std::thread::sleep(std::time::Duration::from_millis(400));

        // Copy executable to C:\Program Files\Helios\helios-host.exe if launched from elsewhere (e.g. Downloads)
        let current_exe = std::env::current_exe()?;
        let target_exe = PathBuf::from(INSTALL_EXE);
        if current_exe.canonicalize().ok() != target_exe.canonicalize().ok() {
            std::fs::copy(&current_exe, &target_exe)
                .with_context(|| format!("failed to copy binary to {}", target_exe.display()))?;
        }

        // Register auto-starting elevated background task running in the interactive desktop session
        let task_args = format!(
            "--identity \"{}\" --name \"{}\" run --background --relay \"{}\"",
            identity_path.display(),
            identity.name,
            relay_url
        );
        let escaped_args = task_args.replace('"', "`\"");
        let ps_register = format!(
            "$action = New-ScheduledTaskAction -Execute '{INSTALL_EXE}' -Argument \"{escaped_args}\"; \
             $trigger = New-ScheduledTaskTrigger -AtLogOn; \
             $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew; \
             $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest; \
             Register-ScheduledTask -TaskName '{TASK_NAME}' -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null; \
             Start-ScheduledTask -TaskName '{TASK_NAME}'"
        );

        let out = Command::new("powershell")
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps_register])
            .output()
            .context("failed to register HeliosHost scheduled task")?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("PowerShell Register-ScheduledTask failed: {stderr}");
        }

        // Copy passcode to Windows clipboard for convenience
        let _ = set_host_clipboard_text(&identity.access_passcode);

        let msg = format!(
            "Helios Host is installed and running silently in the background!\n\n\
             Host Name:  {}\n\
             Host ID:  {}\n\
             Access Passcode:  {}\n\n\
             (The Access Passcode has been copied to your clipboard and saved to:\n\
             C:\\ProgramData\\Helios\\ACCESS_INFO.txt)\n\n\
             It will start automatically every time Windows boots.\n\
             Open HeliosViewer on your Mac and enter this Access Passcode to connect!",
            identity.name, identity.device_id, identity.access_passcode
        );
        show_info_box("Helios Remote Desktop Host — Ready!", &msg);
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        if !is_elevated() {
            let _ = relaunch_elevated("uninstall");
            return Ok(());
        }
        hide_console_window();
        let _ = Command::new("schtasks")
            .args(["/End", "/TN", TASK_NAME])
            .output();
        let _ = Command::new("schtasks")
            .args(["/Delete", "/TN", TASK_NAME, "/F"])
            .output();
        let current_pid = std::process::id();
        let ps_kill = format!(
            "Get-Process helios-host -ErrorAction SilentlyContinue | Where-Object {{ $_.Id -ne {current_pid} }} | Stop-Process -Force -ErrorAction SilentlyContinue"
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps_kill])
            .output();
        show_info_box(
            "Helios Host Uninstalled",
            "Helios Host background process and auto-start task have been removed.",
        );
        Ok(())
    }
}
