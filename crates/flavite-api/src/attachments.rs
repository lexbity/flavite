//! Bounded local image decoding on read workers. No URL fetch path exists.
use std::io::Cursor;
use std::sync::Arc;
pub const MAX_ATTACHMENT_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 8 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadImage {
    pub hash: [u8; 32],
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}
pub fn decode_image(bytes: &[u8]) -> Result<ReadImage, &'static str> {
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err("Image exceeds attachment limit");
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "Unsupported image")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits.clone());
    let (width, height) = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "Unsupported image")?
        .into_dimensions()
        .map_err(|_| "Invalid image")?;
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err("Image exceeds pixel limit");
    }
    let rgba = reader
        .decode()
        .map_err(|_| "Image unavailable or exceeds decode limit")?
        .into_rgba8();
    Ok(ReadImage {
        hash: *blake3::hash(bytes).as_bytes(),
        width,
        height,
        rgba: rgba.into_raw().into(),
    })
}
