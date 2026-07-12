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

/// Convert only the `(rx, ry, rw, rh)` rectangle of `src` (with `stride` bytes
/// per row) into the matching region of `dst`, an existing tightly-packed
/// `width * height * 4` RGBA8 buffer. This is the damage-tracking fast path: a
/// keypress echo in a terminal damages a few cells, so converting just that
/// rectangle replaces a full-frame conversion. The rectangle is clamped to the
/// buffer bounds; a degenerate rectangle is a no-op.
#[allow(clippy::too_many_arguments)]
pub fn convert_rect_into(
    dst: &mut [u8],
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    format: ShmFormat,
    (rx, ry, rw, rh): (usize, usize, usize, usize),
) {
    if dst.len() < width * height * 4 {
        return;
    }
    let x0 = rx.min(width);
    let y0 = ry.min(height);
    let x1 = rx.saturating_add(rw).min(width);
    let y1 = ry.saturating_add(rh).min(height);
    for y in y0..y1 {
        let row_start = y * stride;
        let Some(row) = src.get(row_start + x0 * 4..row_start + x1 * 4) else {
            break; // truncated buffer: leave the rest untouched
        };
        let out_row = &mut dst[(y * width + x0) * 4..(y * width + x1) * 4];
        for x in 0..x1 - x0 {
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
            // Straight-alpha "source over": both color and alpha must weigh the
            // destination by its own alpha, otherwise blending onto a
            // not-fully-opaque canvas (e.g. the transparent flattening canvas)
            // leaves premultiplied color values in a straight-alpha buffer.
            let inv = 255 - a;
            let da = dst[d + 3] as u32;
            // out_a scaled by 255 (i.e. a_out * 255); non-zero because a > 0.
            let out_a = a * 255 + da * inv;
            for c in 0..3 {
                let num = sp[c] as u32 * a * 255 + dst[d + c] as u32 * da * inv;
                dst[d + c] = (num / out_a) as u8;
            }
            dst[d + 3] = ((out_a + 127) / 255) as u8;
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

    #[test]
    fn blit_clips_negative_offset() {
        // 2x2 source placed at (-1,-1): only its bottom-right pixel lands at (0,0).
        let mut dst = vec![0u8; 2 * 2 * 4];
        let src = vec![255u8; 2 * 2 * 4];
        blit_over(&mut dst, 2, 2, -1, -1, &src, 2, 2);
        assert_eq!(&dst[0..4], &[255, 255, 255, 255]);
        assert_eq!(&dst[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn blit_alpha_blends_halfway() {
        let mut dst = vec![0u8, 0, 0, 255]; // opaque black, 1px
        let src = [255u8, 255, 255, 128]; // ~50% white
        blit_over(&mut dst, 1, 1, 0, 0, &src, 1, 1);
        // ~50% blend toward white.
        assert!(dst[0] > 120 && dst[0] < 135);
        assert_eq!(dst[3], 255);
    }

    #[test]
    fn blit_onto_transparent_keeps_source_color() {
        // Blending onto a fully transparent canvas must keep the straight-alpha
        // source untouched, not premultiply its color by its own alpha.
        let mut dst = vec![0u8; 4]; // transparent, 1px
        let src = [255u8, 200, 100, 128];
        blit_over(&mut dst, 1, 1, 0, 0, &src, 1, 1);
        assert_eq!(&dst[0..3], &[255, 200, 100]);
        assert_eq!(dst[3], 128);
    }

    #[test]
    fn blit_onto_translucent_weighs_destination_alpha() {
        // 50% white over 50% black: a_out = 0.75, color = (0.5*255)/0.75 = 170.
        let mut dst = vec![0u8, 0, 0, 128];
        let src = [255u8, 255, 255, 128];
        blit_over(&mut dst, 1, 1, 0, 0, &src, 1, 1);
        assert!(dst[0] >= 168 && dst[0] <= 172, "got {}", dst[0]);
        assert!(dst[3] >= 190 && dst[3] <= 193, "got {}", dst[3]);
    }

    #[test]
    fn convert_truncated_buffer_leaves_zeroes() {
        // Claim 2x2 but provide only one row of data.
        let src = [1, 2, 3, 4, 5, 6, 7, 8];
        let out = convert_to_rgba(&src, 2, 2, 8, ShmFormat::Argb8888);
        assert_eq!(&out[0..8], &[3, 2, 1, 4, 7, 6, 5, 8]);
        assert_eq!(&out[8..16], &[0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn blit_fully_offscreen_is_a_noop() {
        let mut dst = vec![0u8; 4 * 4]; // 2x2 opaque-nothing
        let src = [9u8, 9, 9, 255];
        // Far to the right / below and far to the top-left: nothing lands.
        blit_over(&mut dst, 2, 2, 5, 5, &src, 1, 1);
        blit_over(&mut dst, 2, 2, -5, -5, &src, 1, 1);
        assert!(dst.iter().all(|&b| b == 0), "no pixels should be written");
    }

    #[test]
    fn blit_partially_clipped_writes_only_the_visible_corner() {
        // 2x2 source dropped at (1,1) into a 2x2 dest: only its top-left pixel
        // lands, at dest (1,1).
        let mut dst = vec![0u8; 2 * 2 * 4];
        let src = [
            1, 2, 3, 255, 4, 5, 6, 255, // row 0
            7, 8, 9, 255, 10, 11, 12, 255, // row 1
        ];
        blit_over(&mut dst, 2, 2, 1, 1, &src, 2, 2);
        // dest pixel (1,1) is index (1*2 + 1)*4 = 12.
        assert_eq!(&dst[12..16], &[1, 2, 3, 255]);
        // The other three dest pixels are untouched.
        assert_eq!(&dst[0..4], &[0, 0, 0, 0]);
        assert_eq!(&dst[4..8], &[0, 0, 0, 0]);
        assert_eq!(&dst[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn blit_skips_fully_transparent_source_pixels() {
        let mut dst = vec![50u8; 4]; // one opaque-ish grey pixel
        let src = [200u8, 200, 200, 0]; // fully transparent
        blit_over(&mut dst, 1, 1, 0, 0, &src, 1, 1);
        assert_eq!(dst, vec![50, 50, 50, 50], "transparent source leaves dest as-is");
    }

    #[test]
    fn xrgb_conversion_across_multiple_pixels() {
        // Two BGRX pixels; alpha is forced opaque regardless of the X byte.
        let src = [10u8, 20, 30, 0, 40, 50, 60, 7];
        let out = convert_to_rgba(&src, 2, 1, 8, ShmFormat::Xrgb8888);
        assert_eq!(out, vec![30, 20, 10, 255, 60, 50, 40, 255]);
    }

    #[test]
    fn rect_conversion_touches_only_the_rect() {
        // 2x2 BGRA source; convert only the right column into a canary-filled
        // destination: the left column must keep its canary bytes.
        let src = [
            1u8, 1, 1, 255, 2, 2, 2, 255, // row 0: pixels A, B
            3, 3, 3, 255, 4, 4, 4, 255, // row 1: pixels C, D
        ];
        let mut dst = vec![9u8; 2 * 2 * 4];
        convert_rect_into(&mut dst, &src, 2, 2, 8, ShmFormat::Argb8888, (1, 0, 1, 2));
        assert_eq!(&dst[0..4], &[9, 9, 9, 9], "left of row 0 untouched");
        assert_eq!(&dst[4..8], &[2, 2, 2, 255], "B converted");
        assert_eq!(&dst[8..12], &[9, 9, 9, 9], "left of row 1 untouched");
        assert_eq!(&dst[12..16], &[4, 4, 4, 255], "D converted");
        // Out-of-bounds rectangles clamp instead of panicking.
        convert_rect_into(&mut dst, &src, 2, 2, 8, ShmFormat::Argb8888, (5, 5, 9, 9));
    }
}
