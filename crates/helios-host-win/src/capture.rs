use anyhow::Result;
use helios_proto::CursorShape;

/// Captured desktop frame in tightly packed RGBA format plus optional updated cursor bitmap.
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub cursor_shape: Option<CursorShape>,
}

/// Abstract desktop capture interface.
///
/// On Windows this uses DXGI Desktop Duplication (`IDXGIOutputDuplication`) with
/// zero-idle detection (`AcquireNextFrame` timeout -> `Ok(None)`) and hardware
/// pointer shape extraction (`GetFramePointerShape`).
pub trait ScreenCapturer: Send {
    fn capture_rgba(&mut self) -> Result<Option<CapturedFrame>>;
    fn switch_monitor(&mut self, monitor_index: u32) -> Result<()>;
}

/// Deterministic synthetic screen capturer for cross-platform testing.
pub struct SyntheticCapturer {
    width: u32,
    height: u32,
    tick: u32,
    buf: Vec<u8>,
    sent_cursor: bool,
}

impl SyntheticCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        let mut buf = vec![0u8; (width * height * 4) as usize];
        for px in buf.chunks_exact_mut(4) {
            px[0] = 22;
            px[1] = 27;
            px[2] = 34;
            px[3] = 255;
        }
        Self {
            width,
            height,
            tick: 0,
            buf,
            sent_cursor: false,
        }
    }
}

impl ScreenCapturer for SyntheticCapturer {
    fn capture_rgba(&mut self) -> Result<Option<CapturedFrame>> {
        let w = self.width as usize;
        let h = self.height as usize;

        let prev_x = ((self.tick.saturating_sub(1) * 16) % (self.width.saturating_sub(32).max(1)))
            as usize;
        let curr_x = ((self.tick * 16) % (self.width.saturating_sub(32).max(1))) as usize;
        let y0 = (h / 2).saturating_sub(12);

        for dy in 0..24.min(h.saturating_sub(y0)) {
            for dx in 0..24.min(w.saturating_sub(prev_x)) {
                let idx = ((y0 + dy) * w + (prev_x + dx)) * 4;
                self.buf[idx] = 22;
                self.buf[idx + 1] = 27;
                self.buf[idx + 2] = 34;
                self.buf[idx + 3] = 255;
            }
            for dx in 0..24.min(w.saturating_sub(curr_x)) {
                let idx = ((y0 + dy) * w + (curr_x + dx)) * 4;
                self.buf[idx] = 56;
                self.buf[idx + 1] = 189;
                self.buf[idx + 2] = 248;
                self.buf[idx + 3] = 255;
            }
        }

        let cursor_shape = if !self.sent_cursor {
            self.sent_cursor = true;
            let mut cursor_rgba = vec![0u8; 16 * 16 * 4];
            for i in 0..16usize {
                let idx = (i * 16 + i) * 4;
                cursor_rgba[idx] = 255;
                cursor_rgba[idx + 1] = 255;
                cursor_rgba[idx + 2] = 255;
                cursor_rgba[idx + 3] = 255;
            }
            zstd::bulk::compress(&cursor_rgba, 3).ok().map(|rgba_zstd| CursorShape {
                shape_id: 1,
                width: 16,
                height: 16,
                hotspot_x: 0,
                hotspot_y: 0,
                rgba_zstd,
            })
        } else {
            None
        };

        self.tick = self.tick.wrapping_add(1);
        Ok(Some(CapturedFrame {
            width: self.width,
            height: self.height,
            rgba: self.buf.clone(),
            cursor_shape,
        }))
    }

    fn switch_monitor(&mut self, _monitor_index: u32) -> Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
pub mod dxgi {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use anyhow::{Context, Result};
    use helios_proto::CursorShape;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Dxgi::{
        DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
        DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
        DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
        IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    };
    use windows::core::Interface;

    use super::{CapturedFrame, ScreenCapturer};

    /// Real Windows DXGI Desktop Duplication capturer with pointer shape extraction
    /// and runtime monitor switching.
    pub struct DxgiCapturer {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        duplication: IDXGIOutputDuplication,
        staging: Option<ID3D11Texture2D>,
        width: u32,
        height: u32,
        monitor_index: u32,
        last_cursor_hash: u64,
        cursor_buf: Vec<u8>,
    }

    unsafe impl Send for DxgiCapturer {}

    impl DxgiCapturer {
        pub fn new(monitor_index: u32) -> Result<Self> {
            unsafe {
                let mut device = None;
                let mut context = None;
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    None,
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
                .context("D3D11CreateDevice failed")?;

                let device = device.context("missing D3D11 device")?;
                let context = context.context("missing D3D11 context")?;
                let (duplication, width, height) = Self::init_duplication(&device, monitor_index)?;

                Ok(Self {
                    device,
                    context,
                    duplication,
                    staging: None,
                    width,
                    height,
                    monitor_index,
                    last_cursor_hash: 0,
                    cursor_buf: Vec::new(),
                })
            }
        }

        unsafe fn init_duplication(
            device: &ID3D11Device,
            monitor_index: u32,
        ) -> Result<(IDXGIOutputDuplication, u32, u32)> {
            unsafe {
                let dxgi_device: IDXGIDevice = device.cast()?;
                let adapter = dxgi_device.GetAdapter()?;
                let output = adapter
                    .EnumOutputs(monitor_index)
                    .context("failed to enumerate DXGI monitor output")?;
                let desc = output.GetDesc()?;
                let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
                let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;
                let output1: IDXGIOutput1 = output.cast()?;
                let duplication = output1
                    .DuplicateOutput(device)
                    .context("DXGI DuplicateOutput failed")?;
                Ok((duplication, width, height))
            }
        }

        fn ensure_staging(&mut self, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D> {
            if let Some(tex) = &self.staging {
                return Ok(tex.clone());
            }
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: desc.SampleDesc,
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex = None;
            unsafe {
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut tex))?;
            }
            let tex = tex.context("CreateTexture2D staging failed")?;
            self.staging = Some(tex.clone());
            Ok(tex)
        }

        unsafe fn extract_cursor_shape(&mut self, buf_size: u32) -> Option<CursorShape> {
            if buf_size == 0 {
                return None;
            }
            self.cursor_buf.resize(buf_size as usize, 0);
            let mut required = 0u32;
            let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let hr = unsafe {
                self.duplication.GetFramePointerShape(
                    buf_size,
                    self.cursor_buf.as_mut_ptr() as *mut _,
                    &mut required,
                    &mut shape_info,
                )
            };
            if hr.is_err() {
                return None;
            }

            let w = shape_info.Width as usize;
            let h = if shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
                (shape_info.Height / 2) as usize
            } else {
                shape_info.Height as usize
            };
            if w == 0 || h == 0 || w > 256 || h > 256 {
                return None;
            }

            let pitch = shape_info.Pitch as usize;
            let mut rgba = vec![0u8; w * h * 4];

            if shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32
                || shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32
            {
                for row in 0..h {
                    let src_row = row * pitch;
                    for col in 0..w {
                        let s = src_row + col * 4;
                        if s + 3 >= self.cursor_buf.len() {
                            break;
                        }
                        let d = (row * w + col) * 4;
                        rgba[d] = self.cursor_buf[s + 2];     // R
                        rgba[d + 1] = self.cursor_buf[s + 1]; // G
                        rgba[d + 2] = self.cursor_buf[s];     // B
                        rgba[d + 3] = if shape_info.Type
                            == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32
                        {
                            if self.cursor_buf[s + 3] == 0 { 255 } else { 0 }
                        } else {
                            self.cursor_buf[s + 3]
                        };
                    }
                }
            } else if shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
                let xor_offset = h * pitch;
                for row in 0..h {
                    for col in 0..w {
                        let byte_idx = row * pitch + (col / 8);
                        let bit_mask = 0x80u8 >> (col % 8);
                        let and_bit =
                            (self.cursor_buf.get(byte_idx).copied().unwrap_or(0xFF) & bit_mask) != 0;
                        let xor_bit = (self
                            .cursor_buf
                            .get(xor_offset + byte_idx)
                            .copied()
                            .unwrap_or(0)
                            & bit_mask)
                            != 0;
                        let d = (row * w + col) * 4;
                        match (and_bit, xor_bit) {
                            (false, false) => {
                                // Opaque black
                                rgba[d..d + 4].copy_from_slice(&[0, 0, 0, 255]);
                            }
                            (false, true) => {
                                // Opaque white
                                rgba[d..d + 4].copy_from_slice(&[255, 255, 255, 255]);
                            }
                            (true, true) => {
                                // Invert (render as high-contrast dark gray on macOS local cursor)
                                rgba[d..d + 4].copy_from_slice(&[40, 40, 40, 255]);
                            }
                            (true, false) => {
                                // Transparent
                                rgba[d..d + 4].copy_from_slice(&[0, 0, 0, 0]);
                            }
                        }
                    }
                }
            }

            let mut hasher = DefaultHasher::new();
            w.hash(&mut hasher);
            h.hash(&mut hasher);
            shape_info.HotSpot.x.hash(&mut hasher);
            shape_info.HotSpot.y.hash(&mut hasher);
            rgba.hash(&mut hasher);
            let hash = hasher.finish();
            if hash == self.last_cursor_hash {
                return None;
            }
            self.last_cursor_hash = hash;

            let rgba_zstd = zstd::bulk::compress(&rgba, 3).ok()?;
            Some(CursorShape {
                shape_id: hash,
                width: w as u16,
                height: h as u16,
                hotspot_x: shape_info.HotSpot.x.max(0) as u16,
                hotspot_y: shape_info.HotSpot.y.max(0) as u16,
                rgba_zstd,
            })
        }
    }

    impl ScreenCapturer for DxgiCapturer {
        fn switch_monitor(&mut self, monitor_index: u32) -> Result<()> {
            unsafe {
                let (dup, w, h) = Self::init_duplication(&self.device, monitor_index)?;
                self.duplication = dup;
                self.width = w;
                self.height = h;
                self.monitor_index = monitor_index;
                self.staging = None;
                self.last_cursor_hash = 0;
                Ok(())
            }
        }

        fn capture_rgba(&mut self) -> Result<Option<CapturedFrame>> {
            unsafe {
                let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;

                let hr = self
                    .duplication
                    .AcquireNextFrame(100, &mut frame_info, &mut resource);
                if let Err(err) = hr {
                    if err.code() == DXGI_ERROR_WAIT_TIMEOUT {
                        return Ok(None);
                    }
                    if err.code() == DXGI_ERROR_ACCESS_LOST {
                        let (dup, w, h) =
                            Self::init_duplication(&self.device, self.monitor_index)?;
                        self.duplication = dup;
                        self.width = w;
                        self.height = h;
                        self.staging = None;
                        return Ok(None);
                    }
                    return Err(err.into());
                }

                let result = (|| -> Result<Option<CapturedFrame>> {
                    let cursor_shape = self.extract_cursor_shape(frame_info.PointerShapeBufferSize);

                    if frame_info.LastPresentTime == 0 {
                        return Ok(None);
                    }
                    let res = resource.context("AcquireNextFrame returned no resource")?;
                    let gpu_tex: ID3D11Texture2D = res.cast()?;
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    gpu_tex.GetDesc(&mut desc);

                    let staging = self.ensure_staging(&desc)?;
                    self.context.CopyResource(&staging, &gpu_tex);

                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    self.context
                        .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

                    let width = desc.Width as usize;
                    let height = desc.Height as usize;
                    let row_pitch = mapped.RowPitch as usize;
                    let src = std::slice::from_raw_parts(
                        mapped.pData as *const u8,
                        row_pitch * height,
                    );

                    let mut rgba = Vec::with_capacity(width * height * 4);
                    for row in 0..height {
                        let row_slice = &src[row * row_pitch..row * row_pitch + width * 4];
                        for px in row_slice.chunks_exact(4) {
                            rgba.push(px[2]);
                            rgba.push(px[1]);
                            rgba.push(px[0]);
                            rgba.push(255);
                        }
                    }

                    self.context.Unmap(&staging, 0);
                    Ok(Some(CapturedFrame {
                        width: desc.Width,
                        height: desc.Height,
                        rgba,
                        cursor_shape,
                    }))
                })();

                let _ = self.duplication.ReleaseFrame();
                result
            }
        }
    }
}

pub fn create_default_capturer(
    monitor_index: u32,
    force_synthetic: bool,
) -> Result<Box<dyn ScreenCapturer>> {
    if force_synthetic {
        return Ok(Box::new(SyntheticCapturer::new(1280, 720)));
    }

    #[cfg(windows)]
    {
        match dxgi::DxgiCapturer::new(monitor_index) {
            Ok(cap) => Ok(Box::new(cap)),
            Err(err) => {
                tracing::warn!(
                    "DXGI capture unavailable ({err}); falling back to synthetic test screen"
                );
                Ok(Box::new(SyntheticCapturer::new(1280, 720)))
            }
        }
    }

    #[cfg(not(windows))]
    {
        let _ = monitor_index;
        Ok(Box::new(SyntheticCapturer::new(1280, 720)))
    }
}
