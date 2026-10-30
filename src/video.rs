//! libmpv software frames.
//!
//! `MPV_RENDER_API_TYPE_SW` with format `rgb0` writes R, G, B, and a padding
//! byte. That padding is not alpha, and it is often 0. Slint treats the
//! fourth byte as alpha, so every pixel is forced opaque after the render.

/// Set the fourth byte of each `rgb0` pixel to 255.
pub fn rgb0_to_rgba(frame: &mut [u8]) {
    let mut index = 3;
    while index < frame.len() {
        frame[index] = 255;
        index += 4;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_byte_becomes_opaque() {
        let mut frame = [255u8, 0, 0, 0, 10, 20, 30, 7];
        rgb0_to_rgba(&mut frame);
        assert_eq!(frame, [255, 0, 0, 255, 10, 20, 30, 255]);
    }
}
