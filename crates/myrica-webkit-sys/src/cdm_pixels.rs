//! Copy padded 8-bit planar CDM output into the NV12 texture upload layout.
pub(super) fn to_nv12(
    source: &[u8],
    width: usize,
    height: usize,
    strides: [usize; 3],
    offsets: [usize; 3],
) -> Result<Vec<u8>, &'static str> {
    if width == 0
        || height == 0
        || width > 1920
        || height > 1080
        || width % 2 != 0
        || height % 2 != 0
    {
        return Err("invalid CDM frame dimensions");
    }
    for plane in 0..3 {
        let (columns, rows) = if plane == 0 {
            (width, height)
        } else {
            (width / 2, height / 2)
        };
        let end = strides[plane]
            .checked_mul(rows - 1)
            .and_then(|n| offsets[plane].checked_add(n))
            .and_then(|n| n.checked_add(columns));
        if strides[plane] < columns || end.is_none_or(|n| n > source.len()) {
            return Err("CDM frame plane exceeds output buffer");
        }
    }
    let luma_size = width * height;
    let mut pixels = vec![0; luma_size + luma_size / 2];
    for row in 0..height {
        let start = offsets[0] + row * strides[0];
        pixels[row * width..(row + 1) * width].copy_from_slice(&source[start..start + width]);
    }
    for row in 0..height / 2 {
        for col in 0..width / 2 {
            let dest = luma_size + row * width + col * 2;
            pixels[dest] = source[offsets[1] + row * strides[1] + col];
            pixels[dest + 1] = source[offsets[2] + row * strides[2] + col];
        }
    }
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn padded_planes_keep_chroma_order() {
        let source = [
            1, 2, 3, 4, 99, 99, 5, 6, 7, 8, 99, 99, 10, 20, 99, 30, 40, 99,
        ];
        assert_eq!(
            to_nv12(&source, 4, 2, [6, 3, 3], [0, 12, 15]).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 10, 30, 20, 40]
        );
    }
    #[test]
    fn malformed_planes_and_dimensions_are_rejected() {
        assert!(to_nv12(&[0; 16], 4, 2, [4, 2, 2], [0, 8, usize::MAX]).is_err());
        assert!(to_nv12(&[0; 16], 4, 2, [3, 2, 2], [0, 8, 10]).is_err());
        assert!(to_nv12(&[0; 16], 3, 2, [4, 2, 2], [0, 8, 10]).is_err());
        assert!(to_nv12(&[0; 16], 0, 0, [0; 3], [0; 3]).is_err());
    }
}
