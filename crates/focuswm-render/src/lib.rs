//! Pure conversion of `wl_shm` buffer contents into tightly-packed RGBA8.
//!
//! This crate has no Slint, GL or Wayland dependencies: it is the pure,
//! unit-testable pixel plumbing shared by the Wayland engine (which extracts and
//! flattens client buffers) and the binary crate (which uploads them to GL). The
//! GL upload itself lives in the binary crate's `gl_bridge` module, where Slint's
//! GL context is current.
//!
//! `wl_shm`'s ARGB8888/XRGB8888 formats are little-endian 32-bit words
//! (`0xAARRGGBB`), i.e. the bytes in memory are `[B, G, R, A]`. Slint's
//! `Rgba8Pixel` wants `[R, G, B, A]`, so we swap and (for XRGB) force alpha.

/// The shm pixel formats we support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShmFormat {
    /// 32-bit ARGB, alpha honored.
    Argb8888,
    /// 32-bit XRGB, alpha ignored (treated as opaque).
    Xrgb8888,
}

/// Convert `src` (with `stride` bytes per row) into a tightly-packed
/// `width * height * 4` RGBA8 buffer. Stride padding past `width*4` is skipped.
pub fn convert_to_rgba(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    format: ShmFormat,
) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let row_start = y * stride;
        let Some(row) = src.get(row_start..row_start + width * 4) else {
            // Truncated/short buffer: leave the remaining rows zeroed.
            break;
        };
        let out_row = &mut out[y * width * 4..(y + 1) * width * 4];
        for x in 0..width {
            let s = &row[x * 4..x * 4 + 4];
            let o = &mut out_row[x * 4..x * 4 + 4];
            o[0] = s[2]; // R
            o[1] = s[1]; // G
            o[2] = s[0]; // B
            o[3] = match format {
                ShmFormat::Argb8888 => s[3],
                ShmFormat::Xrgb8888 => 255,
            };
        }
    }
    out
}

/// Alpha-blend a tightly-packed `src` RGBA8 image of `sw`x`sh` onto a
/// tightly-packed `dst` RGBA8 canvas of `dw`x`dh`, with its top-left corner at
/// `(x, y)` (which may be negative or partly off-canvas — those pixels are
/// clipped). Straight-alpha "source over destination" compositing, used to
/// flatten a `wl_surface` tree (a window plus its subsurfaces) into one buffer.
#[allow(clippy::too_many_arguments)]
pub fn blit_over(
    dst: &mut [u8],
    dw: usize,
    dh: usize,
    x: i32,
    y: i32,
    src: &[u8],
    sw: usize,
    sh: usize,
) {
    for row in 0..sh {
        let dy = y + row as i32;
        if dy < 0 || dy as usize >= dh {
            continue;
        }
        let dy = dy as usize;
        for col in 0..sw {
            let dx = x + col as i32;
            if dx < 0 || dx as usize >= dw {
                continue;
            }
            let dx = dx as usize;
            let s = (row * sw + col) * 4;
            let Some(sp) = src.get(s..s + 4) else {
                continue;
            };
            let a = sp[3] as u32;
            if a == 0 {
                continue;
            }
            let d = (dy * dw + dx) * 4;
            if a == 255 {
                dst[d..d + 4].copy_from_slice(sp);
                continue;
            }
            let inv = 255 - a;
            for c in 0..3 {
                dst[d + c] = ((sp[c] as u32 * a + dst[d + c] as u32 * inv) / 255) as u8;
            }
            dst[d + 3] = (a + dst[d + 3] as u32 * inv / 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_bgra_to_rgba() {
        let src = [10u8, 20, 30, 40];
        let out = convert_to_rgba(&src, 1, 1, 4, ShmFormat::Argb8888);
        assert_eq!(out, vec![30, 20, 10, 40]);
    }

    #[test]
    fn xrgb_forces_opaque_alpha() {
        let src = [10u8, 20, 30, 0];
        let out = convert_to_rgba(&src, 1, 1, 4, ShmFormat::Xrgb8888);
        assert_eq!(out, vec![30, 20, 10, 255]);
    }

    #[test]
    fn skips_stride_padding() {
        let src = [1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0];
        let out = convert_to_rgba(&src, 1, 2, 8, ShmFormat::Argb8888);
        assert_eq!(out, vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn blit_opaque_overwrites() {
        let mut dst = vec![0u8; 2 * 2 * 4];
        let src = [9, 9, 9, 255];
        blit_over(&mut dst, 2, 2, 1, 1, &src, 1, 1);
        // Only the bottom-right pixel is written.
        assert_eq!(&dst[12..16], &[9, 9, 9, 255]);
        assert_eq!(&dst[0..4], &[0, 0, 0, 0]);
    }
}
