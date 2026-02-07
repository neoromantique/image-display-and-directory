use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use image::codecs::gif::GifDecoder;
use image::AnimationDecoder;
use image::{DynamicImage, ImageFormat, ImageReader};

pub fn open_image(path: &Path) -> Result<DynamicImage> {
    open_image_with_rotation(path, 0)
}

pub fn open_image_with_rotation(path: &Path, extra_rotation_cw: u8) -> Result<DynamicImage> {
    let bytes = std::fs::read(path).with_context(|| format!("Failed to read image: {:?}", path))?;
    let format = image::guess_format(&bytes).ok();

    let img = if format == Some(ImageFormat::Gif) {
        let decoder = GifDecoder::new(Cursor::new(&bytes))
            .with_context(|| format!("Failed to decode GIF: {:?}", path))?;
        let mut frames = decoder.into_frames();
        if let Some(frame) = frames.next() {
            let frame = frame.context("Failed to decode GIF frame")?;
            DynamicImage::ImageRgba8(frame.into_buffer())
        } else {
            return Err(anyhow!("GIF has no frames: {:?}", path));
        }
    } else {
        match format {
            Some(fmt) => image::load_from_memory_with_format(&bytes, fmt)
                .with_context(|| format!("Failed to decode image: {:?}", path))?,
            None => image::load_from_memory(&bytes)
                .with_context(|| format!("Failed to decode image: {:?}", path))?,
        }
    };

    let orientation = read_exif_orientation_from_bytes(&bytes).unwrap_or(1);
    let img = apply_exif_orientation(img, orientation);
    Ok(apply_rotation_steps(img, extra_rotation_cw))
}

pub fn read_dimensions(path: &Path) -> Result<(u32, u32)> {
    let reader = ImageReader::open(path)
        .with_context(|| format!("Failed to open image: {:?}", path))?
        .with_guessed_format()
        .context("Failed to guess image format")?;

    let mut dims = if reader.format() == Some(ImageFormat::Gif) {
        let bytes =
            std::fs::read(path).with_context(|| format!("Failed to read GIF: {:?}", path))?;
        let decoder = GifDecoder::new(Cursor::new(bytes))
            .with_context(|| format!("Failed to decode GIF: {:?}", path))?;
        let mut frames = decoder.into_frames();
        if let Some(frame) = frames.next() {
            let frame = frame.context("Failed to decode GIF frame")?;
            let buf = frame.into_buffer();
            (buf.width(), buf.height())
        } else {
            return Err(anyhow!("GIF has no frames: {:?}", path));
        }
    } else {
        reader
            .into_dimensions()
            .with_context(|| format!("Failed to read dimensions: {:?}", path))?
    };

    if let Some(orientation) = read_exif_orientation_from_path(path) {
        if needs_dimension_swap(orientation) {
            dims = (dims.1, dims.0);
        }
    }

    Ok(dims)
}

/// Try to load the embedded EXIF JPEG thumbnail (if present) and apply orientation/rotation.
/// Returns (preview_image, original_width, original_height).
pub fn open_embedded_jpeg_preview_with_rotation(
    path: &Path,
    extra_rotation_cw: u8,
) -> Option<(DynamicImage, u32, u32)> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; 256 * 1024];
    let read = file.read(&mut buf).ok()?;
    let bytes = &buf[..read];
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }

    let thumb_bytes = parse_jpeg_exif_thumbnail(bytes)?;
    let mut img = image::load_from_memory_with_format(thumb_bytes, ImageFormat::Jpeg).ok()?;

    let orientation = read_exif_orientation_from_bytes(bytes).unwrap_or(1);
    img = apply_exif_orientation(img, orientation);
    img = apply_rotation_steps(img, extra_rotation_cw);

    let (orig_w, orig_h) = read_dimensions(path).ok()?;
    Some((img, orig_w, orig_h))
}

fn apply_rotation_steps(img: DynamicImage, extra_rotation_cw: u8) -> DynamicImage {
    match extra_rotation_cw % 4 {
        0 => img,
        1 => img.rotate90(),
        2 => img.rotate180(),
        _ => img.rotate270(),
    }
}

fn apply_exif_orientation(img: DynamicImage, orientation: u16) -> DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        // Transpose: flip across the top-left -> bottom-right diagonal.
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        // Transverse: flip across the top-right -> bottom-left diagonal.
        7 => img.rotate90().flipv(),
        8 => img.rotate270(),
        _ => img,
    }
}

fn needs_dimension_swap(orientation: u16) -> bool {
    matches!(orientation, 5..=8)
}

fn read_exif_orientation_from_path(path: &Path) -> Option<u16> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; 256 * 1024];
    let read = file.read(&mut buf).ok()?;
    read_exif_orientation_from_bytes(&buf[..read])
}

fn read_exif_orientation_from_bytes(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 4 {
        return None;
    }

    if bytes.starts_with(&[0xFF, 0xD8]) {
        return parse_jpeg_exif_orientation(bytes);
    }

    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return parse_tiff_orientation(bytes);
    }

    None
}

fn parse_jpeg_exif_orientation(bytes: &[u8]) -> Option<u16> {
    let mut pos = 2usize;

    while pos + 1 < bytes.len() {
        while pos < bytes.len() && bytes[pos] == 0xFF {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }

        let marker = bytes[pos];
        pos += 1;

        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }

        if pos + 2 > bytes.len() {
            break;
        }
        let segment_len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;

        if segment_len < 2 {
            break;
        }
        let payload_len = segment_len - 2;
        if pos + payload_len > bytes.len() {
            break;
        }

        if marker == 0xE1 {
            let segment = &bytes[pos..pos + payload_len];
            if segment.starts_with(b"Exif\0\0") && segment.len() >= 6 {
                if let Some(orientation) = parse_tiff_orientation(&segment[6..]) {
                    return Some(orientation);
                }
            }
        }

        pos += payload_len;
    }

    None
}

fn parse_jpeg_exif_thumbnail(bytes: &[u8]) -> Option<&[u8]> {
    let mut pos = 2usize;

    while pos + 1 < bytes.len() {
        while pos < bytes.len() && bytes[pos] == 0xFF {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }

        let marker = bytes[pos];
        pos += 1;

        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }

        if pos + 2 > bytes.len() {
            break;
        }
        let segment_len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        if segment_len < 2 {
            break;
        }
        let payload_len = segment_len - 2;
        if pos + payload_len > bytes.len() {
            break;
        }

        if marker == 0xE1 {
            let segment = &bytes[pos..pos + payload_len];
            if segment.starts_with(b"Exif\0\0") && segment.len() >= 6 {
                let tiff = &segment[6..];
                if let Some((offset, length)) = parse_tiff_jpeg_thumbnail(tiff) {
                    let end = offset.saturating_add(length);
                    if end <= tiff.len() {
                        return Some(&tiff[offset..end]);
                    }
                }
            }
        }

        pos += payload_len;
    }

    None
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

fn parse_tiff_orientation(tiff: &[u8]) -> Option<u16> {
    if tiff.len() < 8 {
        return None;
    }

    let endian = match &tiff[0..2] {
        b"II" => Endian::Little,
        b"MM" => Endian::Big,
        _ => return None,
    };

    if read_u16(tiff, 2, endian)? != 42 {
        return None;
    }

    let ifd_offset = read_u32(tiff, 4, endian)? as usize;
    if ifd_offset + 2 > tiff.len() {
        return None;
    }

    let entry_count = read_u16(tiff, ifd_offset, endian)? as usize;
    let entries_start = ifd_offset + 2;

    for idx in 0..entry_count {
        let entry = entries_start + idx * 12;
        if entry + 12 > tiff.len() {
            break;
        }

        let tag = read_u16(tiff, entry, endian)?;
        if tag != 0x0112 {
            continue;
        }

        let field_type = read_u16(tiff, entry + 2, endian)?;
        let count = read_u32(tiff, entry + 4, endian)?;
        if count == 0 {
            continue;
        }

        let value = match (field_type, count) {
            (3, 1) => read_u16(tiff, entry + 8, endian)?,
            (3, _) => {
                let offset = read_u32(tiff, entry + 8, endian)? as usize;
                read_u16(tiff, offset, endian)?
            }
            _ => continue,
        };

        if (1..=8).contains(&value) {
            return Some(value);
        }
    }

    None
}

fn parse_tiff_jpeg_thumbnail(tiff: &[u8]) -> Option<(usize, usize)> {
    if tiff.len() < 8 {
        return None;
    }

    let endian = match &tiff[0..2] {
        b"II" => Endian::Little,
        b"MM" => Endian::Big,
        _ => return None,
    };

    if read_u16(tiff, 2, endian)? != 42 {
        return None;
    }

    let ifd0_offset = read_u32(tiff, 4, endian)? as usize;
    let ifd0_count = read_u16(tiff, ifd0_offset, endian)? as usize;
    let ifd0_after_entries = ifd0_offset.checked_add(2 + ifd0_count * 12)?;
    let ifd1_offset = read_u32(tiff, ifd0_after_entries, endian)? as usize;
    if ifd1_offset == 0 {
        return None;
    }

    let ifd1_count = read_u16(tiff, ifd1_offset, endian)? as usize;
    let ifd1_entries = ifd1_offset.checked_add(2)?;

    let mut thumb_offset: Option<u32> = None;
    let mut thumb_length: Option<u32> = None;

    for idx in 0..ifd1_count {
        let entry = ifd1_entries.checked_add(idx * 12)?;
        if entry + 12 > tiff.len() {
            break;
        }
        let tag = read_u16(tiff, entry, endian)?;
        if tag == 0x0201 {
            thumb_offset = read_ifd_u32(tiff, entry, endian);
        } else if tag == 0x0202 {
            thumb_length = read_ifd_u32(tiff, entry, endian);
        }
    }

    let offset = thumb_offset? as usize;
    let length = thumb_length? as usize;
    if length == 0 {
        return None;
    }
    Some((offset, length))
}

fn read_ifd_u32(tiff: &[u8], entry: usize, endian: Endian) -> Option<u32> {
    let field_type = read_u16(tiff, entry + 2, endian)?;
    let count = read_u32(tiff, entry + 4, endian)?;
    match (field_type, count) {
        (3, 1) => Some(read_u16(tiff, entry + 8, endian)? as u32),
        (4, 1) => read_u32(tiff, entry + 8, endian),
        (3, _) => {
            let offset = read_u32(tiff, entry + 8, endian)? as usize;
            Some(read_u16(tiff, offset, endian)? as u32)
        }
        (4, _) => {
            let offset = read_u32(tiff, entry + 8, endian)? as usize;
            read_u32(tiff, offset, endian)
        }
        _ => None,
    }
}

fn read_u16(data: &[u8], offset: usize, endian: Endian) -> Option<u16> {
    let bytes: [u8; 2] = data.get(offset..offset + 2)?.try_into().ok()?;
    Some(match endian {
        Endian::Little => u16::from_le_bytes(bytes),
        Endian::Big => u16::from_be_bytes(bytes),
    })
}

fn read_u32(data: &[u8], offset: usize, endian: Endian) -> Option<u32> {
    let bytes: [u8; 4] = data.get(offset..offset + 4)?.try_into().ok()?;
    Some(match endian {
        Endian::Little => u32::from_le_bytes(bytes),
        Endian::Big => u32::from_be_bytes(bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tiff_orientation() {
        // Minimal little-endian TIFF with one IFD entry: Orientation=6 (rotate 90 CW)
        let tiff = vec![
            b'I', b'I', 42, 0, 8, 0, 0, 0, // header + IFD0 offset
            1, 0, // entry count
            0x12, 0x01, // tag 0x0112
            3, 0, // type SHORT
            1, 0, 0, 0, // count
            6, 0, 0, 0, // value
            0, 0, 0, 0, // next IFD
        ];

        assert_eq!(parse_tiff_orientation(&tiff), Some(6));
        assert!(needs_dimension_swap(6));
    }
}
