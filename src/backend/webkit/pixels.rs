//! Cairo surface validation and premultiplied BGRA to straight RGBA conversion.

pub(super) const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

pub fn convert_bgra(
    width: u32,
    height: u32,
    stride: usize,
    bytes: &[u8],
) -> Result<Vec<u8>, &'static str> {
    let invalid = || "WebCore returned an invalid pixel surface";
    if width == 0 || height == 0 {
        return Err(invalid());
    }
    let row_bytes = (width as usize).checked_mul(4).ok_or_else(invalid)?;
    let buffer_bytes = stride.checked_mul(height as usize).ok_or_else(invalid)?;
    let output_bytes = row_bytes.checked_mul(height as usize).ok_or_else(invalid)?;
    if stride < row_bytes || bytes.len() < buffer_bytes || output_bytes > MAX_FRAME_BYTES {
        return Err(invalid());
    }
    let mut rgba = Vec::with_capacity(output_bytes);
    for row in bytes[..buffer_bytes].chunks_exact(stride) {
        for pixel in row[..row_bytes].chunks_exact(4) {
            let alpha = u32::from(pixel[3]);
            let straight = |value: u8| {
                if alpha == 0 {
                    0
                } else {
                    ((u32::from(value) * 255 + alpha / 2) / alpha).min(255) as u8
                }
            };
            rgba.extend_from_slice(&[
                straight(pixel[2]),
                straight(pixel[1]),
                straight(pixel[0]),
                pixel[3],
            ]);
        }
    }
    Ok(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cairo_bgra_stride_and_alpha_are_converted_for_sgfx() {
        let surface = [
            10, 20, 30, 255, 0xAA, 0xAA, 0xAA, 0xAA, 16, 32, 64, 128, 0xBB, 0xBB, 0xBB, 0xBB,
        ];
        assert_eq!(
            convert_bgra(1, 2, 8, &surface).unwrap(),
            [30, 20, 10, 255, 128, 64, 32, 128]
        );
        assert_eq!(convert_bgra(1, 1, 4, &[5, 6, 7, 0]).unwrap(), [0, 0, 0, 0]);
    }

    #[test]
    fn malformed_surfaces_are_rejected_before_reading_pixels() {
        assert!(convert_bgra(2, 1, 4, &[0; 8]).is_err());
        assert!(convert_bgra(2, 2, 8, &[0; 8]).is_err());
        assert!(convert_bgra(0, 1, 4, &[0; 4]).is_err());
        assert!(convert_bgra(1, 2, usize::MAX, &[]).is_err());
    }
}
