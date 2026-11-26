//! Ultra-low-bandwidth **Survival / Text Mode** dirty-tile diff & Zstd codec.
//!
//! When internet speed drops below ~150 kbps or packet loss spikes, traditional
//! H.264/H.265 video streams turn text into unreadable blur or stall on large IDR
//! keyframes. This module divides the desktop into 64x64 pixel tiles, skips all
//! unchanged tiles (0 bytes when static!), optionally applies mild color
//! quantization for even higher compression, and compresses only the changed
//! tiles with Zstd so terminals, IDEs, and UI text stay 100% razor-sharp.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_TILE_SIZE: u32 = 64;

#[derive(Debug, Error)]
pub enum TileError {
    #[error("framebuffer size mismatch: expected {expected} bytes, got {actual}")]
    SizeMismatch { expected: usize, actual: usize },
    #[error("zstd compression/decompression error: {0}")]
    Zstd(#[from] std::io::Error),
    #[error("tile out of bounds at ({x}, {y}) size ({w}x{h}) for frame ({fw}x{fh})")]
    OutOfBounds {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        fw: u32,
        fh: u32,
    },
}

/// A single changed rectangular tile compressed with Zstd.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirtyTile {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Zstd-compressed tightly packed RGBA bytes (`width * height * 4`).
    pub rgba_zstd: Vec<u8>,
}

/// Collection of changed tiles for one desktop frame update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirtyTileFrame {
    pub frame_id: u64,
    pub timestamp_us: u64,
    pub frame_width: u32,
    pub frame_height: u32,
    pub is_full_refresh: bool,
    pub tiles: Vec<DirtyTile>,
}

/// Stateful dirty-tile encoder running on the Host.
pub struct TileEncoder {
    tile_size: u32,
    zstd_level: i32,
    /// Optional bit shift per color channel (`0` = lossless, `2` = 6-bit RGB for tiny payloads).
    quantize_shift: u8,
    prev_width: u32,
    prev_height: u32,
    prev_rgba: Vec<u8>,
    next_frame_id: u64,
}

impl TileEncoder {
    pub fn new(tile_size: u32, zstd_level: i32, quantize_shift: u8) -> Self {
        Self {
            tile_size: tile_size.max(16),
            zstd_level,
            quantize_shift: quantize_shift.min(4),
            prev_width: 0,
            prev_height: 0,
            prev_rgba: Vec::new(),
            next_frame_id: 1,
        }
    }

    pub fn reset(&mut self) {
        self.prev_width = 0;
        self.prev_height = 0;
        self.prev_rgba.clear();
    }

    /// Compare `curr_rgba` against the previous frame and return `Ok(None)` when
    /// zero pixels changed (0 bandwidth used!), or `Ok(Some(DirtyTileFrame))`
    /// containing only the changed tiles compressed with Zstd.
    pub fn encode_rgba(
        &mut self,
        width: u32,
        height: u32,
        curr_rgba: &[u8],
        timestamp_us: u64,
        force_full: bool,
    ) -> Result<Option<DirtyTileFrame>, TileError> {
        let expected = (width as usize) * (height as usize) * 4;
        if curr_rgba.len() != expected {
            return Err(TileError::SizeMismatch {
                expected,
                actual: curr_rgba.len(),
            });
        }

        let is_full_refresh = force_full
            || self.prev_width != width
            || self.prev_height != height
            || self.prev_rgba.len() != expected;

        let mut tiles = Vec::new();
        let stride = (width as usize) * 4;
        let mut tile_buf = Vec::with_capacity((self.tile_size * self.tile_size * 4) as usize);

        let mut y = 0;
        while y < height {
            let th = self.tile_size.min(height - y);
            let mut x = 0;
            while x < width {
                let tw = self.tile_size.min(width - x);
                let changed = if is_full_refresh {
                    true
                } else {
                    tile_differs(
                        &self.prev_rgba,
                        curr_rgba,
                        stride,
                        x as usize,
                        y as usize,
                        tw as usize,
                        th as usize,
                    )
                };

                if changed {
                    tile_buf.clear();
                    for row in 0..(th as usize) {
                        let start = ((y as usize) + row) * stride + (x as usize) * 4;
                        let end = start + (tw as usize) * 4;
                        if self.quantize_shift == 0 {
                            tile_buf.extend_from_slice(&curr_rgba[start..end]);
                        } else {
                            let mask = !((1u8 << self.quantize_shift) - 1);
                            for px in curr_rgba[start..end].chunks_exact(4) {
                                tile_buf.push(px[0] & mask);
                                tile_buf.push(px[1] & mask);
                                tile_buf.push(px[2] & mask);
                                tile_buf.push(255);
                            }
                        }
                    }

                    let rgba_zstd = zstd::bulk::compress(&tile_buf, self.zstd_level)?;
                    tiles.push(DirtyTile {
                        x,
                        y,
                        width: tw,
                        height: th,
                        rgba_zstd,
                    });
                }

                x += self.tile_size;
            }
            y += self.tile_size;
        }

        if is_full_refresh {
            self.prev_width = width;
            self.prev_height = height;
            self.prev_rgba.resize(expected, 0);
            self.prev_rgba.copy_from_slice(curr_rgba);
        } else if !tiles.is_empty() {
            // Update only changed tiles in reference buffer
            for t in &tiles {
                for row in 0..(t.height as usize) {
                    let start = ((t.y as usize) + row) * stride + (t.x as usize) * 4;
                    let end = start + (t.width as usize) * 4;
                    self.prev_rgba[start..end].copy_from_slice(&curr_rgba[start..end]);
                }
            }
        }

        if tiles.is_empty() {
            return Ok(None);
        }

        let frame_id = self.next_frame_id;
        self.next_frame_id += 1;

        Ok(Some(DirtyTileFrame {
            frame_id,
            timestamp_us,
            frame_width: width,
            frame_height: height,
            is_full_refresh,
            tiles,
        }))
    }
}

fn tile_differs(
    prev: &[u8],
    curr: &[u8],
    stride: usize,
    x: usize,
    y: usize,
    tw: usize,
    th: usize,
) -> bool {
    let row_bytes = tw * 4;
    for row in 0..th {
        let start = (y + row) * stride + x * 4;
        let end = start + row_bytes;
        if prev[start..end] != curr[start..end] {
            return true;
        }
    }
    false
}

/// Apply a `DirtyTileFrame` onto the client's persistent RGBA framebuffer.
pub fn apply_tile_frame(
    frame: &DirtyTileFrame,
    dst_width: &mut u32,
    dst_height: &mut u32,
    dst_rgba: &mut Vec<u8>,
) -> Result<(), TileError> {
    let expected = (frame.frame_width as usize) * (frame.frame_height as usize) * 4;
    if *dst_width != frame.frame_width
        || *dst_height != frame.frame_height
        || dst_rgba.len() != expected
    {
        *dst_width = frame.frame_width;
        *dst_height = frame.frame_height;
        dst_rgba.resize(expected, 0);
    }

    let stride = (frame.frame_width as usize) * 4;
    for tile in &frame.tiles {
        if tile.x + tile.width > frame.frame_width || tile.y + tile.height > frame.frame_height {
            return Err(TileError::OutOfBounds {
                x: tile.x,
                y: tile.y,
                w: tile.width,
                h: tile.height,
                fw: frame.frame_width,
                fh: frame.frame_height,
            });
        }
        let max_tile_bytes = (tile.width as usize) * (tile.height as usize) * 4;
        let raw = zstd::bulk::decompress(&tile.rgba_zstd, max_tile_bytes)?;
        if raw.len() != max_tile_bytes {
            return Err(TileError::SizeMismatch {
                expected: max_tile_bytes,
                actual: raw.len(),
            });
        }

        let row_bytes = (tile.width as usize) * 4;
        for row in 0..(tile.height as usize) {
            let src_start = row * row_bytes;
            let dst_start = ((tile.y as usize) + row) * stride + (tile.x as usize) * 4;
            dst_rgba[dst_start..dst_start + row_bytes]
                .copy_from_slice(&raw[src_start..src_start + row_bytes]);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_frame_emits_zero_bytes_and_dirty_tile_roundtrips() {
        let width = 256u32;
        let height = 128u32;
        let mut screen = vec![30u8; (width * height * 4) as usize];

        let mut encoder = TileEncoder::new(DEFAULT_TILE_SIZE, 3, 0);

        // First frame is full refresh (4x2 = 8 tiles)
        let f1 = encoder
            .encode_rgba(width, height, &screen, 1000, false)
            .unwrap()
            .expect("first frame must emit tiles");
        assert!(f1.is_full_refresh);
        assert_eq!(f1.tiles.len(), 8);

        // Identical frame must emit None (0 kbps idle!)
        let f2 = encoder
            .encode_rgba(width, height, &screen, 2000, false)
            .unwrap();
        assert!(f2.is_none());

        // Modify a single pixel inside tile (1, 0) -> x=70, y=10
        let idx = ((10 * width + 70) * 4) as usize;
        screen[idx] = 255;
        screen[idx + 1] = 128;
        screen[idx + 2] = 64;
        screen[idx + 3] = 255;

        let f3 = encoder
            .encode_rgba(width, height, &screen, 3000, false)
            .unwrap()
            .expect("dirty frame must emit 1 tile");
        assert!(!f3.is_full_refresh);
        assert_eq!(f3.tiles.len(), 1);
        assert_eq!(f3.tiles[0].x, 64);
        assert_eq!(f3.tiles[0].y, 0);

        // Reconstruct on client side
        let mut cw = 0;
        let mut ch = 0;
        let mut client_fb = Vec::new();
        apply_tile_frame(&f1, &mut cw, &mut ch, &mut client_fb).unwrap();
        apply_tile_frame(&f3, &mut cw, &mut ch, &mut client_fb).unwrap();
        assert_eq!(client_fb, screen);
    }

    #[test]
    fn full_hd_typing_update_stays_tiny_for_slow_links() {
        let w = 1920u32;
        let h = 1080u32;
        let mut screen = vec![24u8; (w * h * 4) as usize];
        let mut enc = TileEncoder::new(DEFAULT_TILE_SIZE, 3, 1);

        // Initial full refresh
        let _ = enc.encode_rgba(w, h, &screen, 1, false).unwrap().unwrap();

        // Simulate typing a character block (20x24 px) inside a terminal window
        for dy in 0..24u32 {
            for dx in 0..20u32 {
                let idx = (((400 + dy) * w + (300 + dx)) * 4) as usize;
                screen[idx] = 220;
                screen[idx + 1] = 230;
                screen[idx + 2] = 240;
                screen[idx + 3] = 255;
            }
        }

        let delta = enc
            .encode_rgba(w, h, &screen, 2, false)
            .unwrap()
            .expect("typing must emit dirty tile");
        assert_eq!(delta.tiles.len(), 1);
        let compressed_bytes: usize = delta.tiles.iter().map(|t| t.rgba_zstd.len()).sum();
        // Even at 5 FPS, < 2 KB per typing update is < 80 kbps (and fits in a single QUIC datagram!)
        assert!(
            compressed_bytes < 1024,
            "expected <1 KB for single typing tile, got {compressed_bytes} bytes"
        );
    }
}
