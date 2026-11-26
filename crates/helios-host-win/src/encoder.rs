//! Hybrid Low-Bandwidth Desktop Encoder for Helios Windows Host.
//!
//! Automatically switches between:
//! 1. **Survival / Text Mode (`TileEncoder`)**: 64x64 Zstd dirty-tile diffs when
//!    bandwidth < 150 kbps, packet loss > 5%, or the user toggles Survival Mode.
//! 2. **Hardware H.264 Mode (`MfH264Encoder` on Windows)**: Uses Windows Media Foundation
//!    Hardware MFTs (automatically selecting NVIDIA NVENC, AMD AMF, or Intel QSV)
//!    tuned for ultra-low-latency (0 B-frames, dynamic bitrate, Annex-B output for
//!    Apple VideoToolbox hardware decoding on macOS).

use anyhow::Result;
use helios_proto::{MediaMessage, QualityTier, TileEncoder, VideoFramePacket};
use tracing::{info, warn};

/// Convert tightly packed RGBA8888 into planar/semi-planar NV12 (Y plane + interleaved UV plane)
/// using fast BT.601 integer math. Requires even width & height (odd trailing pixel is clamped).
pub fn rgba_to_nv12(width: u32, height: u32, rgba: &[u8], out: &mut Vec<u8>) {
    let w = (width & !1) as usize;
    let h = (height & !1) as usize;
    let stride = (width as usize) * 4;
    let y_len = w * h;
    let uv_len = y_len / 2;

    out.resize(y_len + uv_len, 0);
    let (y_plane, uv_plane) = out.split_at_mut(y_len);

    for row in 0..h {
        let src_row = row * stride;
        let y_row = row * w;
        for col in 0..w {
            let p = src_row + col * 4;
            let r = rgba[p] as i32;
            let g = rgba[p + 1] as i32;
            let b = rgba[p + 2] as i32;
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[y_row + col] = y.clamp(16, 235) as u8;
        }
    }

    for row in (0..h).step_by(2) {
        let src0 = row * stride;
        let src1 = (row + 1) * stride;
        let uv_row = (row / 2) * w;
        for col in (0..w).step_by(2) {
            let p00 = src0 + col * 4;
            let p01 = p00 + 4;
            let p10 = src1 + col * 4;
            let p11 = p10 + 4;

            let r = (rgba[p00] as i32 + rgba[p01] as i32 + rgba[p10] as i32 + rgba[p11] as i32) >> 2;
            let g = (rgba[p00 + 1] as i32
                + rgba[p01 + 1] as i32
                + rgba[p10 + 1] as i32
                + rgba[p11 + 1] as i32)
                >> 2;
            let b = (rgba[p00 + 2] as i32
                + rgba[p01 + 2] as i32
                + rgba[p10 + 2] as i32
                + rgba[p11 + 2] as i32)
                >> 2;

            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            uv_plane[uv_row + col] = u.clamp(16, 240) as u8;
            uv_plane[uv_row + col + 1] = v.clamp(16, 240) as u8;
        }
    }
}

/// Abstract hardware/software Annex-B video encoder.
pub trait HardwareVideoEncoder: Send {
    fn encode_rgba(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
        timestamp_us: u64,
        force_keyframe: bool,
    ) -> Result<Option<VideoFramePacket>>;

    fn reconfigure(&mut self, bitrate_kbps: u32, fps: u32) -> Result<()>;
}

/// Hybrid encoder owned by the Host's capture thread.
pub struct HybridEncoder {
    tile_encoder: TileEncoder,
    video_encoder: Option<Box<dyn HardwareVideoEncoder>>,
    last_tier: Option<QualityTier>,
    last_was_tiles: bool,
    keyframe_requested: bool,
}

impl HybridEncoder {
    pub fn new(initial_width: u32, initial_height: u32) -> Self {
        let video_encoder = create_platform_video_encoder(initial_width, initial_height);
        if video_encoder.is_some() {
            info!("initialized hybrid encoder (Hardware H.264 + Survival Zstd Dirty-Tiles)");
        } else {
            info!("initialized encoder in Zstd Dirty-Tile mode (no HW video encoder active)");
        }
        Self {
            tile_encoder: TileEncoder::new(64, 3, 1),
            video_encoder,
            last_tier: None,
            last_was_tiles: true,
            keyframe_requested: true,
        }
    }

    pub fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    pub fn encode_frame(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
        timestamp_us: u64,
        tier: QualityTier,
    ) -> Result<Option<MediaMessage>> {
        let want_tiles = tier.uses_dirty_tiles() || self.video_encoder.is_none();

        if want_tiles {
            let force_full = !self.last_was_tiles || self.keyframe_requested;
            self.last_was_tiles = true;
            self.last_tier = Some(tier);
            self.keyframe_requested = false;
            return Ok(self
                .tile_encoder
                .encode_rgba(width, height, rgba, timestamp_us, force_full)?
                .map(MediaMessage::DirtyTiles));
        }

        // Video mode (H.264 Annex-B for VideoToolbox decoding on macOS)
        if let Some(vid) = self.video_encoder.as_mut() {
            if self.last_tier != Some(tier) {
                let _ = vid.reconfigure(tier.target_bitrate_kbps(), tier.target_fps());
                self.last_tier = Some(tier);
            }
            let force_kf = self.last_was_tiles || self.keyframe_requested;
            self.last_was_tiles = false;
            self.keyframe_requested = false;

            match vid.encode_rgba(width, height, rgba, timestamp_us, force_kf) {
                Ok(Some(pkt)) => return Ok(Some(MediaMessage::VideoFrame(pkt))),
                Ok(None) => return Ok(None),
                Err(err) => {
                    warn!("HW video encode failed ({err}); falling back to dirty-tile frame");
                    self.tile_encoder.reset();
                    return Ok(self
                        .tile_encoder
                        .encode_rgba(width, height, rgba, timestamp_us, true)?
                        .map(MediaMessage::DirtyTiles));
                }
            }
        }

        Ok(None)
    }
}

#[cfg(windows)]
mod mf_h264 {
    use anyhow::{Context, Result, bail};
    use helios_proto::{VideoCodec, VideoFramePacket};
    use windows::Win32::Media::MediaFoundation::{
        CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncMPVDefaultBPictureCount,
        CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
        IMFMediaType, IMFSample, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT,
        MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
        MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE,
        MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFStartup,
        MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
        MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_FLUSH,
        MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_INFO,
        MFMediaType_Video, MFSTARTUP_NOSOCKET, MFVideoFormat_H264, MFVideoFormat_NV12,
        MFVideoInterlace_Progressive, MFT_REGISTER_TYPE_INFO, eAVEncH264VProfile_Main,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::core::{Interface, VARIANT};

    use super::{HardwareVideoEncoder, rgba_to_nv12};

    fn pack_u64(hi: u32, lo: u32) -> u64 {
        ((hi as u64) << 32) | (lo as u64)
    }

    /// Windows Media Foundation Hardware H.264 Encoder (NVENC / AMD AMF / Intel QSV).
    pub struct MfH264Encoder {
        transform: IMFTransform,
        codec_api: Option<ICodecAPI>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        frame_id: u64,
        nv12_buf: Vec<u8>,
    }

    unsafe impl Send for MfH264Encoder {}

    impl MfH264Encoder {
        pub fn new(width: u32, height: u32, bitrate_kbps: u32, fps: u32) -> Result<Self> {
            let even_w = (width & !1).max(64);
            let even_h = (height & !1).max(64);
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).context("MFStartup failed")?;

                let output_info = MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_H264,
                };

                let mut activates_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
                let mut count: u32 = 0;

                // Prefer GPU hardware H.264 MFT (NVENC / AMF / QSV); fall back to sync SW MFT
                MFTEnumEx(
                    MFT_CATEGORY_VIDEO_ENCODER,
                    MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                    None,
                    Some(&output_info),
                    &mut activates_ptr,
                    &mut count,
                )
                .context("MFTEnumEx for H.264 video encoder failed")?;

                if count == 0 || activates_ptr.is_null() {
                    bail!("no Windows Media Foundation H.264 encoder MFT found");
                }

                let slice = std::slice::from_raw_parts(activates_ptr, count as usize);
                let activate = slice[0]
                    .as_ref()
                    .context("first MFT activate entry was null")?
                    .clone();
                let transform: IMFTransform = activate
                    .ActivateObject()
                    .context("ActivateObject for H.264 MFT failed")?;
                CoTaskMemFree(Some(activates_ptr as *const _));

                let codec_api = transform.cast::<ICodecAPI>().ok();
                let bitrate_bps = bitrate_kbps * 1000;

                // Configure output H.264 Main Profile Annex-B stream
                let out_type: IMFMediaType = MFCreateMediaType()?;
                out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
                out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(even_w, even_h))?;
                out_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps.max(1), 1))?;
                out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
                transform
                    .SetOutputType(0, &out_type, 0)
                    .context("MFT SetOutputType(H264) failed")?;

                // Configure input NV12 stream
                let in_type: IMFMediaType = MFCreateMediaType()?;
                in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
                in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(even_w, even_h))?;
                in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps.max(1), 1))?;
                transform
                    .SetInputType(0, &in_type, 0)
                    .context("MFT SetInputType(NV12) failed")?;

                // Tune for ultra-low-latency remote desktop (0 B-frames, low-latency mode)
                if let Some(api) = &codec_api {
                    let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                    let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
                    let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate_bps));
                }

                transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;

                Ok(Self {
                    transform,
                    codec_api,
                    width: even_w,
                    height: even_h,
                    fps: fps.max(1),
                    bitrate_bps,
                    frame_id: 1,
                    nv12_buf: Vec::new(),
                })
            }
        }
    }

    impl HardwareVideoEncoder for MfH264Encoder {
        fn reconfigure(&mut self, bitrate_kbps: u32, fps: u32) -> Result<()> {
            self.bitrate_bps = bitrate_kbps * 1000;
            self.fps = fps.max(1);
            if let Some(api) = &self.codec_api {
                unsafe {
                    let _ = api.SetValue(
                        &CODECAPI_AVEncCommonMeanBitRate,
                        &VARIANT::from(self.bitrate_bps),
                    );
                }
            }
            Ok(())
        }

        fn encode_rgba(
            &mut self,
            width: u32,
            height: u32,
            rgba: &[u8],
            timestamp_us: u64,
            force_keyframe: bool,
        ) -> Result<Option<VideoFramePacket>> {
            let even_w = width & !1;
            let even_h = height & !1;
            if even_w != self.width || even_h != self.height {
                // Re-initialize MFT if desktop resolution changed
                *self = Self::new(even_w, even_h, self.bitrate_bps / 1000, self.fps)?;
            }

            rgba_to_nv12(width, height, rgba, &mut self.nv12_buf);

            unsafe {
                if force_keyframe && let Some(api) = &self.codec_api {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }

                let buffer = MFCreateMemoryBuffer(self.nv12_buf.len() as u32)?;
                let mut ptr: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut ptr, None, None)?;
                std::ptr::copy_nonoverlapping(self.nv12_buf.as_ptr(), ptr, self.nv12_buf.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(self.nv12_buf.len() as u32)?;

                let sample: IMFSample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime((timestamp_us * 10) as i64)?; // 100ns units
                sample.SetSampleDuration((10_000_000 / self.fps.max(1) as u64) as i64)?;

                self.transform.ProcessInput(0, &sample, 0)?;

                let out_info: MFT_OUTPUT_STREAM_INFO = self.transform.GetOutputStreamInfo(0)?;
                let out_sample: IMFSample = MFCreateSample()?;
                let out_buf = MFCreateMemoryBuffer(out_info.cbSize.max(512 * 1024))?;
                out_sample.AddBuffer(&out_buf)?;

                let mut out_data = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(Some(out_sample)),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }];
                let mut status: u32 = 0;

                let hr = self.transform.ProcessOutput(0, &mut out_data, &mut status);
                let produced_sample = std::mem::ManuallyDrop::into_inner(out_data[0].pSample);
                let _ = std::mem::ManuallyDrop::into_inner(out_data[0].pEvents);

                match hr {
                    Ok(()) => {
                        let Some(samp) = produced_sample else {
                            return Ok(None);
                        };
                        let contig = samp.ConvertToContiguousBuffer()?;
                        let mut p: *mut u8 = std::ptr::null_mut();
                        let mut cur_len: u32 = 0;
                        contig.Lock(&mut p, None, Some(&mut cur_len))?;
                        let payload = std::slice::from_raw_parts(p, cur_len as usize).to_vec();
                        contig.Unlock()?;

                        let is_keyframe = contains_h264_idr(&payload);
                        let frame_id = self.frame_id;
                        self.frame_id += 1;

                        Ok(Some(VideoFramePacket {
                            frame_id,
                            timestamp_us,
                            width: self.width,
                            height: self.height,
                            codec: VideoCodec::H264,
                            is_keyframe,
                            payload,
                        }))
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                        Ok(None)
                    }
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    fn contains_h264_idr(annex_b: &[u8]) -> bool {
        let mut i = 0;
        while i + 4 < annex_b.len() {
            if annex_b[i] == 0 && annex_b[i + 1] == 0 {
                let (nal_idx, step) = if annex_b[i + 2] == 1 {
                    (i + 3, 3)
                } else if annex_b[i + 2] == 0 && annex_b[i + 3] == 1 {
                    (i + 4, 4)
                } else {
                    i += 1;
                    continue;
                };
                if nal_idx < annex_b.len() && (annex_b[nal_idx] & 0x1F) == 5 {
                    return true;
                }
                i += step;
            } else {
                i += 1;
            }
        }
        false
    }
}

fn create_platform_video_encoder(width: u32, height: u32) -> Option<Box<dyn HardwareVideoEncoder>> {
    #[cfg(windows)]
    {
        match mf_h264::MfH264Encoder::new(width, height, 1500, 24) {
            Ok(enc) => Some(Box::new(enc)),
            Err(err) => {
                warn!("Windows Media Foundation H.264 encoder unavailable ({err}); using dirty-tile mode");
                None
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (width, height);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_to_nv12_converts_dimensions_and_neutral_gray() {
        let w = 64u32;
        let h = 32u32;
        // Neutral mid-gray (128, 128, 128, 255) -> U=128, V=128
        let mut rgba = vec![128u8; (w * h * 4) as usize];
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let mut nv12 = Vec::new();
        rgba_to_nv12(w, h, &rgba, &mut nv12);

        assert_eq!(nv12.len(), (w * h * 3 / 2) as usize);
        let y_len = (w * h) as usize;
        // Mid-gray Y ≈ 126, U/V = 128
        assert!((nv12[0] as i32 - 126).abs() <= 2);
        assert_eq!(nv12[y_len], 128);
        assert_eq!(nv12[y_len + 1], 128);
    }
}
