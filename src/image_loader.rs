use std::io::Cursor;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use image::codecs::gif::GifDecoder;
use image::{DynamicImage, ImageFormat, ImageReader};
use image::AnimationDecoder;

pub fn open_image(path: &Path) -> Result<DynamicImage> {
    let bytes = std::fs::read(path).with_context(|| format!("Failed to read image: {:?}", path))?;
    let format = image::guess_format(&bytes).ok();

    if format == Some(ImageFormat::Gif) {
        let decoder = GifDecoder::new(Cursor::new(bytes))
            .with_context(|| format!("Failed to decode GIF: {:?}", path))?;
        let mut frames = decoder.into_frames();
        if let Some(frame) = frames.next() {
            let frame = frame.context("Failed to decode GIF frame")?;
            return Ok(DynamicImage::ImageRgba8(frame.into_buffer()));
        }
        return Err(anyhow!("GIF has no frames: {:?}", path));
    }

    match format {
        Some(fmt) => image::load_from_memory_with_format(&bytes, fmt)
            .with_context(|| format!("Failed to decode image: {:?}", path)),
        None => image::load_from_memory(&bytes)
            .with_context(|| format!("Failed to decode image: {:?}", path)),
    }
}

pub fn read_dimensions(path: &Path) -> Result<(u32, u32)> {
    let bytes = std::fs::read(path).with_context(|| format!("Failed to read image: {:?}", path))?;
    let format = image::guess_format(&bytes).ok();

    if format == Some(ImageFormat::Gif) {
        let decoder = GifDecoder::new(Cursor::new(bytes))
            .with_context(|| format!("Failed to decode GIF: {:?}", path))?;
        let mut frames = decoder.into_frames();
        if let Some(frame) = frames.next() {
            let frame = frame.context("Failed to decode GIF frame")?;
            let buf = frame.into_buffer();
            return Ok((buf.width(), buf.height()));
        }
        return Err(anyhow!("GIF has no frames: {:?}", path));
    }

    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("Failed to guess image format")?;
    reader
        .into_dimensions()
        .with_context(|| format!("Failed to read dimensions: {:?}", path))
}
