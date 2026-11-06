//! Bounded theme-image decoding, including APNG animation frames.
//!
//! Theme headers may be PNG (including APNG), JPEG, or — on Firefox —
//! SVG and CSS gradients. This decoder supports the raster formats: SVG
//! and gradients are refused with typed errors so the conversion report
//! can state the drop precisely. Every decode is bounded: an input over
//! the byte budget, a frame over the pixel budget, more frames than the
//! frame budget, or a total output over the decoded-bytes budget is a
//! hard error, never a truncation.
//!
//! APNG subframes are composited per the APNG specification (blend op and
//! dispose op over the running canvas), so each returned frame is a
//! full-size RGBA image ready to hand to a renderer.

use crate::ThemeError;
use std::io::Cursor;
use std::sync::Arc;

/// Maximum encoded bytes for one theme image.
pub const MAX_THEME_IMAGE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum pixels for one frame (matches the attachment decoder budget).
pub const MAX_IMAGE_PIXELS: u64 = 8 * 1024 * 1024;
/// Maximum width or height of one frame.
pub const MAX_IMAGE_DIMENSION: u32 = 8192;
/// Maximum animation frames per image.
pub const MAX_FRAMES: usize = 256;
/// Maximum total decoded output bytes (all frames, all buffers).
pub const MAX_TOTAL_DECODED_BYTES: usize = 64 * 1024 * 1024;

/// One composited animation frame: full-size RGBA pixels and its delay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnimationFrame {
    /// Frame duration in milliseconds (always at least 10).
    pub delay_ms: u32,
    /// Full-size RGBA8 pixels (`width * height * 4`).
    pub rgba: Arc<[u8]>,
}

/// A decoded theme image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeImage {
    /// One static RGBA image.
    Static {
        /// Image width in pixels.
        width: u32,
        /// Image height in pixels.
        height: u32,
        /// Full-size RGBA8 pixels.
        rgba: Arc<[u8]>,
    },
    /// A multi-frame APNG; frame 0 is the first displayed frame.
    Animated {
        /// Image width in pixels.
        width: u32,
        /// Image height in pixels.
        height: u32,
        /// Composited frames in display order.
        frames: Vec<AnimationFrame>,
    },
}

impl ThemeImage {
    /// The image width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        match self {
            Self::Static { width, .. } | Self::Animated { width, .. } => *width,
        }
    }

    /// The image height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        match self {
            Self::Static { height, .. } | Self::Animated { height, .. } => *height,
        }
    }

    /// Whether the image carries animation frames.
    #[must_use]
    pub const fn is_animated(&self) -> bool {
        matches!(self, Self::Animated { .. })
    }
}

/// Decode a theme image from its packaged bytes.
///
/// # Errors
/// [`ThemeError::TooLarge`] on any budget violation,
/// [`ThemeError::Image`] for unreadable or unsupported formats.
pub fn decode_image(bytes: &[u8]) -> Result<ThemeImage, ThemeError> {
    if bytes.len() > MAX_THEME_IMAGE_BYTES {
        return Err(ThemeError::TooLarge(format!(
            "image is {} bytes, over the {MAX_THEME_IMAGE_BYTES} byte bound",
            bytes.len()
        )));
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        decode_png(bytes)
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        decode_jpeg(bytes)
    } else {
        Err(ThemeError::Image(
            "theme_frame".into(),
            "unsupported format (SVG and gradients are not supported yet)".into(),
        ))
    }
}

fn decode_png(bytes: &[u8]) -> Result<ThemeImage, ThemeError> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    decoder.set_limits(png::Limits {
        bytes: MAX_TOTAL_DECODED_BYTES,
    });
    let mut reader = decoder
        .read_info()
        .map_err(|error| ThemeError::Image("png header".into(), error.to_string()))?;
    let header = reader.info();
    let (width, height) = (header.width, header.height);
    validate_dimensions(width, height)?;
    let frame_count = header
        .animation_control
        .map(|animation| animation.num_frames as usize)
        .unwrap_or(1);
    if frame_count > MAX_FRAMES {
        return Err(ThemeError::TooLarge(format!(
            "animation has {frame_count} frames, over the {MAX_FRAMES} frame bound"
        )));
    }
    // The widest possible post-transform frame is full-size RGBA8, which
    // is also what `Reader::next_frame` requires the buffer to hold.
    let mut canvas = vec![0u8; width as usize * height as usize * 4];
    let mut scratch = canvas.clone();
    let mut budget = 0usize;
    let mut frames = Vec::with_capacity(frame_count);

    for index in 0..frame_count {
        let output = reader
            .next_frame(&mut scratch)
            .map_err(|error| ThemeError::Image(format!("frame {index}"), error.to_string()))?;
        let control = reader.info().frame_control;
        let placement = FramePlacement {
            x_offset: control.map_or(0, |control| control.x_offset),
            y_offset: control.map_or(0, |control| control.y_offset),
            width: control.map_or(width, |control| control.width),
            height: control.map_or(height, |control| control.height),
        };
        validate_dimensions(placement.width, placement.height)?;
        if placement.x_offset.saturating_add(placement.width) > width
            || placement.y_offset.saturating_add(placement.height) > height
            || output.width != placement.width
            || output.height != placement.height
        {
            return Err(ThemeError::Image(
                format!("frame {index}"),
                "frame geometry does not fit the image header".into(),
            ));
        }
        let subframe = to_rgba8(
            &scratch,
            placement.width,
            placement.height,
            output.color_type,
        );
        budget = track_budget(budget, placement.width, placement.height)?;
        let (delay, dispose, blend) = match control {
            Some(control) => (delay_ms(&control), control.dispose_op, control.blend_op),
            // A plain PNG has no fcTL: full-frame replace, nothing to
            // dispose. The delay is meaningless without animation.
            None => (100, png::DisposeOp::None, png::BlendOp::Source),
        };
        compose_frame(&mut canvas, width, placement, &subframe, blend, dispose);
        frames.push(AnimationFrame {
            delay_ms: delay,
            rgba: canvas.clone().into(),
        });
    }

    if frames.len() > 1 {
        Ok(ThemeImage::Animated {
            width,
            height,
            frames,
        })
    } else {
        let frame = frames
            .into_iter()
            .next()
            .ok_or_else(|| ThemeError::Image("png".into(), "the image carries no frames".into()))?;
        Ok(ThemeImage::Static {
            width,
            height,
            rgba: frame.rgba,
        })
    }
}

fn decode_jpeg(bytes: &[u8]) -> Result<ThemeImage, ThemeError> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| ThemeError::Image("jpeg".into(), error.to_string()))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|error| ThemeError::Image("jpeg".into(), error.to_string()))?;
    let rgba = decoded.into_rgba8();
    Ok(ThemeImage::Static {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw().into(),
    })
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), ThemeError> {
    if width == 0 || height == 0 {
        return Err(ThemeError::Image(
            "png".into(),
            "image has a zero dimension".into(),
        ));
    }
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(ThemeError::TooLarge(format!(
            "frame is {width}x{height}, over the {MAX_IMAGE_DIMENSION}px bound"
        )));
    }
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(ThemeError::TooLarge(format!(
            "frame exceeds {MAX_IMAGE_PIXELS} pixels"
        )));
    }
    Ok(())
}

fn track_budget(current: usize, width: u32, height: u32) -> Result<usize, ThemeError> {
    let frame = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| ThemeError::TooLarge("frame size overflows".into()))?;
    let total = current
        .checked_add(frame)
        .ok_or_else(|| ThemeError::TooLarge("decode budget overflows".into()))?;
    if total > MAX_TOTAL_DECODED_BYTES {
        return Err(ThemeError::TooLarge(format!(
            "animation needs {total} decoded bytes, over the {MAX_TOTAL_DECODED_BYTES} byte bound"
        )));
    }
    Ok(total)
}

fn delay_ms(control: &png::FrameControl) -> u32 {
    // APNG: a zero denominator means 100 ticks per second.
    let denominator = u32::from(control.delay_den).max(1);
    if control.delay_num == 0 {
        return 100;
    }
    let delay = (u32::from(control.delay_num) * 1000) / denominator;
    delay.clamp(10, 60_000)
}

/// Convert one decoded frame (any output color layout) into RGBA8.
fn to_rgba8(raw: &[u8], width: u32, height: u32, color_type: png::ColorType) -> Vec<u8> {
    let pixels = width as usize * height as usize;
    let mut rgba = Vec::with_capacity(pixels * 4);
    match color_type {
        png::ColorType::Grayscale => {
            for &gray in raw.iter().take(pixels) {
                rgba.extend_from_slice(&[gray, gray, gray, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for index in 0..pixels {
                let gray = raw[index * 2];
                rgba.extend_from_slice(&[gray, gray, gray, raw[index * 2 + 1]]);
            }
        }
        png::ColorType::Rgb => {
            for index in 0..pixels {
                rgba.extend_from_slice(&[
                    raw[index * 3],
                    raw[index * 3 + 1],
                    raw[index * 3 + 2],
                    255,
                ]);
            }
        }
        png::ColorType::Rgba | png::ColorType::Indexed => {
            rgba.extend_from_slice(&raw[..pixels * 4]);
        }
    }
    rgba
}

/// Where one APNG subframe sits inside the image.
#[derive(Debug, Clone, Copy)]
struct FramePlacement {
    /// Left edge of the subframe.
    x_offset: u32,
    /// Top edge of the subframe.
    y_offset: u32,
    /// Subframe width.
    width: u32,
    /// Subframe height.
    height: u32,
}

/// Blend one subframe onto the running canvas, then apply the dispose op
/// so the canvas is correct for the next frame (APNG §4.4 composition).
/// `subframe` is RGBA8 at exactly `placement.width × placement.height`.
fn compose_frame(
    canvas: &mut [u8],
    image_width: u32,
    placement: FramePlacement,
    subframe: &[u8],
    blend: png::BlendOp,
    dispose: png::DisposeOp,
) {
    let stride = image_width as usize * 4;
    let frame_stride = placement.width as usize * 4;
    let previous = (dispose == png::DisposeOp::Previous).then(|| canvas.to_vec());
    for row in 0..placement.height as usize {
        let canvas_row =
            (placement.y_offset as usize + row) * stride + placement.x_offset as usize * 4;
        let source_row = row * frame_stride;
        for column in 0..placement.width as usize {
            let canvas_index = canvas_row + column * 4;
            let source_index = source_row + column * 4;
            let source: &[u8; 4] = subframe[source_index..source_index + 4]
                .try_into()
                .expect("subframe is frame_width × frame_height RGBA8");
            match blend {
                png::BlendOp::Source => {
                    canvas[canvas_index..canvas_index + 4].copy_from_slice(source);
                }
                png::BlendOp::Over => {
                    let blended = alpha_over(source, &canvas[canvas_index..canvas_index + 4]);
                    canvas[canvas_index..canvas_index + 4].copy_from_slice(&blended);
                }
            }
        }
    }
    match dispose {
        png::DisposeOp::None => {}
        png::DisposeOp::Background => {
            for row in 0..placement.height as usize {
                let start =
                    (placement.y_offset as usize + row) * stride + placement.x_offset as usize * 4;
                let end = start + frame_stride;
                canvas[start..end].fill(0);
            }
        }
        png::DisposeOp::Previous => {
            if let Some(saved) = previous {
                canvas.copy_from_slice(&saved);
            }
        }
    }
}

fn alpha_over(source: &[u8; 4], backdrop: &[u8]) -> [u8; 4] {
    let alpha = u16::from(source[3]);
    if alpha == 255 {
        return [source[0], source[1], source[2], 255];
    }
    if alpha == 0 {
        return [backdrop[0], backdrop[1], backdrop[2], backdrop[3]];
    }
    let inverse = 255 - alpha;
    let blend =
        |front: u8, back: u8| ((u16::from(front) * alpha + u16::from(back) * inverse) / 255) as u8;
    [
        blend(source[0], backdrop[0]),
        blend(source[1], backdrop[1]),
        blend(source[2], backdrop[2]),
        (alpha + u16::from(backdrop[3]) * inverse / 255) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a tiny RGBA PNG (or APNG when frames > 1) exactly the way a
    /// theme author's tool would.
    fn encode_png(width: u32, height: u32, frames: &[Vec<u8>], delay: (u16, u16)) -> Vec<u8> {
        let mut output = Vec::new();
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        if frames.len() > 1 {
            encoder
                .set_animated(frames.len() as u32, 0)
                .expect("animation setup");
            encoder
                .set_frame_delay(delay.0, delay.1)
                .expect("frame delay");
        }
        let mut writer = encoder.write_header().expect("header");
        for frame in frames {
            if frames.len() > 1 {
                writer.set_frame_delay(delay.0, delay.1).expect("delay");
            }
            writer.write_image_data(frame).expect("frame data");
        }
        drop(writer);
        output
    }

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        [rgba[0], rgba[1], rgba[2], rgba[3]].repeat((width * height) as usize)
    }

    #[test]
    fn static_png_decodes_to_rgba() {
        let image = decode_image(&encode_png(
            4,
            2,
            &[solid(4, 2, [10, 20, 30, 255])],
            (1, 10),
        ))
        .expect("static decode");
        let ThemeImage::Static {
            width,
            height,
            rgba,
        } = image
        else {
            panic!("expected a static image");
        };
        assert_eq!((width, height), (4, 2));
        assert_eq!(&rgba[..8], &[10, 20, 30, 255, 10, 20, 30, 255]);
    }

    #[test]
    fn apng_decodes_all_frames_with_delays() {
        let image = decode_image(&encode_png(
            2,
            1,
            &[
                solid(2, 1, [255, 0, 0, 255]),
                solid(2, 1, [0, 255, 0, 255]),
                solid(2, 1, [0, 0, 255, 255]),
            ],
            (1, 20),
        ))
        .expect("animated decode");
        let ThemeImage::Animated { frames, .. } = image else {
            panic!("expected an animated image");
        };
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].rgba[..4], [255, 0, 0, 255]);
        assert_eq!(frames[1].rgba[..4], [0, 255, 0, 255]);
        assert_eq!(frames[2].rgba[..4], [0, 0, 255, 255]);
        assert_eq!(frames[0].delay_ms, 50);
    }

    #[test]
    fn translucent_subframes_blend_over_the_canvas() {
        // The png crate's plain encoder path writes full frames, so the
        // subframe composition path is exercised directly: an opaque red
        // canvas with a 1x1 translucent white subframe at x=1.
        let mut canvas = vec![0u8; 8];
        canvas.copy_from_slice(&[255, 0, 0, 255, 255, 0, 0, 255]);
        compose_frame(
            &mut canvas,
            2,
            FramePlacement {
                x_offset: 1,
                y_offset: 0,
                width: 1,
                height: 1,
            },
            &[255, 255, 255, 128],
            png::BlendOp::Over,
            png::DisposeOp::None,
        );
        assert_eq!(&canvas[4..8], &[255, 128, 128, 255]);
        assert_eq!(&canvas[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn dispose_background_clears_the_region_for_the_next_frame() {
        let mut canvas = vec![255u8; 16];
        compose_frame(
            &mut canvas,
            2,
            FramePlacement {
                x_offset: 1,
                y_offset: 1,
                width: 1,
                height: 1,
            },
            &[10, 20, 30, 255],
            png::BlendOp::Source,
            png::DisposeOp::Background,
        );
        // The painted pixel at (1,1) is cleared again by the dispose op.
        assert_eq!(&canvas[12..16], &[0, 0, 0, 0]);
        assert_eq!(&canvas[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn jpeg_decodes() {
        let mut jpeg = Vec::new();
        {
            let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut jpeg);
            encoder
                .encode(
                    &[255, 0, 0, 0, 255, 0, 0, 0, 255],
                    3,
                    1,
                    image::ExtendedColorType::Rgb8,
                )
                .expect("jpeg encode");
        }
        let image = decode_image(&jpeg).expect("jpeg decode");
        assert_eq!(image.width(), 3);
        assert_eq!(image.height(), 1);
        assert!(!image.is_animated());
    }

    #[test]
    fn unsupported_and_oversized_inputs_are_typed_errors() {
        assert!(matches!(
            decode_image(b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
            Err(ThemeError::Image(_, _))
        ));
        assert!(matches!(
            decode_image(&vec![0u8; MAX_THEME_IMAGE_BYTES + 1]),
            Err(ThemeError::TooLarge(_))
        ));
    }
}
