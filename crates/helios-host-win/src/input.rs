use anyhow::Result;
use helios_proto::InputEvent;
use tracing::debug;

pub trait InputInjector: Send {
    fn inject(&mut self, event: &InputEvent) -> Result<()>;
}

/// Cross-platform no-op/logging injector used when running host tests on Linux/macOS.
pub struct LoggingInputInjector;

impl InputInjector for LoggingInputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        debug!(?event, "received remote input event (stub injector)");
        Ok(())
    }
}

#[cfg(windows)]
pub mod win_input {
    use anyhow::Result;
    use helios_proto::{InputEvent, MouseButton};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
        MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
        MOUSEINPUT, SendInput, VIRTUAL_KEY,
    };

    use super::InputInjector;

    /// Real Windows `SendInput` mouse & keyboard injector.
    pub struct Win32InputInjector;

    impl Win32InputInjector {
        fn send_mouse(dx: i32, dy: i32, mouse_data: u32, flags: MOUSE_EVENT_FLAGS) {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx,
                        dy,
                        mouseData: mouse_data,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            unsafe {
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
        }
    }

    impl InputInjector for Win32InputInjector {
        fn inject(&mut self, event: &InputEvent) -> Result<()> {
            match *event {
                InputEvent::MouseMoveAbs { x, y } => {
                    let abs_x = (x.clamp(0.0, 1.0) * 65535.0) as i32;
                    let abs_y = (y.clamp(0.0, 1.0) * 65535.0) as i32;
                    Self::send_mouse(
                        abs_x,
                        abs_y,
                        0,
                        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    );
                }
                InputEvent::MouseMoveRel { dx, dy } => {
                    Self::send_mouse(dx, dy, 0, MOUSEEVENTF_MOVE);
                }
                InputEvent::MouseButton { button, pressed } => {
                    let flag = match (button, pressed) {
                        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                        _ => return Ok(()),
                    };
                    Self::send_mouse(0, 0, 0, flag);
                }
                InputEvent::MouseWheel { dx, dy } => {
                    if dy != 0 {
                        Self::send_mouse(0, 0, (dy * 120) as u32, MOUSEEVENTF_WHEEL);
                    }
                    if dx != 0 {
                        Self::send_mouse(0, 0, (dx * 120) as u32, MOUSEEVENTF_HWHEEL);
                    }
                }
                InputEvent::Key { code, pressed } => {
                    let flags = if pressed {
                        KEYBD_EVENT_FLAGS(0)
                    } else {
                        KEYEVENTF_KEYUP
                    };
                    let input = INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: VIRTUAL_KEY(code.0),
                                wScan: 0,
                                dwFlags: flags,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    };
                    unsafe {
                        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                    }
                }
                InputEvent::SendCtrlAltDel => {
                    unsafe {
                        use windows::Win32::Foundation::BOOL;
                        use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
                        use windows::core::{s, w};

                        if let Ok(module) = LoadLibraryW(w!("sas.dll")) {
                            if let Some(proc_fn) = GetProcAddress(module, s!("SendSAS")) {
                                let send_sas: extern "system" fn(BOOL) =
                                    std::mem::transmute(proc_fn);
                                send_sas(BOOL(0)); // FALSE = called from Windows Service
                                tracing::info!("invoked Windows SendSAS(FALSE) for Ctrl+Alt+Del");
                            }
                        } else {
                            tracing::warn!("sas.dll unavailable; SendSAS requires Windows Service / LocalSystem");
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

pub fn create_default_injector() -> Box<dyn InputInjector> {
    #[cfg(windows)]
    {
        Box::new(win_input::Win32InputInjector)
    }
    #[cfg(not(windows))]
    {
        Box::new(LoggingInputInjector)
    }
}
