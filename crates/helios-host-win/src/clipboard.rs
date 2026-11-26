//! On-demand UTF-8 text clipboard sync for the Windows Host.
//!
//! Only transmits text when explicitly requested by the macOS Viewer (never
//! auto-uploads large screenshots or binaries over slow connections).

use anyhow::Result;

const MAX_CLIPBOARD_BYTES: usize = 65_536;

pub fn set_host_clipboard_text(text: &str) -> Result<()> {
    let capped = if text.len() > MAX_CLIPBOARD_BYTES {
        &text[..text.floor_char_boundary(MAX_CLIPBOARD_BYTES)]
    } else {
        text
    };

    #[cfg(windows)]
    {
        win_clip::set_unicode_text(capped)
    }

    #[cfg(not(windows))]
    {
        tracing::info!(bytes = capped.len(), "set_host_clipboard_text (non-Windows stub)");
        Ok(())
    }
}

pub fn get_host_clipboard_text() -> Result<Option<String>> {
    #[cfg(windows)]
    {
        win_clip::get_unicode_text(MAX_CLIPBOARD_BYTES)
    }

    #[cfg(not(windows))]
    {
        Ok(Some("Helios Host clipboard (non-Windows stub)".into()))
    }
}

#[cfg(windows)]
mod win_clip {
    use anyhow::{Context, Result};
    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };

    const CF_UNICODETEXT: u32 = 13;

    pub fn set_unicode_text(text: &str) -> Result<()> {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let byte_len = wide.len() * 2;

        unsafe {
            OpenClipboard(HWND::default()).context("OpenClipboard failed")?;
            let result = (|| -> Result<()> {
                EmptyClipboard().context("EmptyClipboard failed")?;
                let hmem: HGLOBAL =
                    GlobalAlloc(GMEM_MOVEABLE, byte_len).context("GlobalAlloc failed")?;
                let ptr = GlobalLock(hmem) as *mut u16;
                if ptr.is_null() {
                    anyhow::bail!("GlobalLock returned null");
                }
                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                let _ = GlobalUnlock(hmem);
                SetClipboardData(CF_UNICODETEXT, HANDLE(hmem.0))
                    .context("SetClipboardData failed")?;
                Ok(())
            })();
            let _ = CloseClipboard();
            result
        }
    }

    pub fn get_unicode_text(max_bytes: usize) -> Result<Option<String>> {
        unsafe {
            if OpenClipboard(HWND::default()).is_err() {
                return Ok(None);
            }
            let result = (|| -> Result<Option<String>> {
                let Ok(handle) = GetClipboardData(CF_UNICODETEXT) else {
                    return Ok(None);
                };
                if handle.is_invalid() {
                    return Ok(None);
                }
                let ptr = GlobalLock(HGLOBAL(handle.0)) as *const u16;
                if ptr.is_null() {
                    return Ok(None);
                }
                let mut len = 0usize;
                let max_u16 = max_bytes / 2;
                while len < max_u16 && *ptr.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(ptr, len);
                let text = String::from_utf16_lossy(slice);
                let _ = GlobalUnlock(HGLOBAL(handle.0));
                Ok(Some(text))
            })();
            let _ = CloseClipboard();
            result
        }
    }
}
