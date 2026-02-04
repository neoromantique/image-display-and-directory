//! Metadata extraction for media files.
//!
//! This module provides fast extraction of media dimensions and other metadata,
//! reading only the necessary headers when possible to minimize I/O.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result};
use image::ImageReader;
use tracing::{debug, trace, warn};

use crate::models::MediaType;

/// Result of metadata extraction for a media file.
#[derive(Debug, Clone)]
pub struct MediaMetadata {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Duration in milliseconds (only for videos).
    pub duration_ms: Option<u32>,
    /// Whether extraction encountered non-fatal issues.
    pub has_warnings: bool,
}

/// Error state marker for broken media files.
pub const ERROR_DIMENSION: u32 = 0;

/// Extracts metadata from a media file based on its type.
pub struct MetadataExtractor;

impl MetadataExtractor {
    /// Extracts dimensions from an image or video file.
    ///
    /// For images, this attempts to read only the header to get dimensions quickly.
    /// For videos, it uses a basic approach (can be enhanced with ffprobe later).
    ///
    /// Returns `(0, 0)` for broken/unreadable files instead of erroring,
    /// allowing the application to display a placeholder.
    pub fn extract_dimensions(path: &Path) -> Result<(u32, u32)> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        match MediaType::from_extension(&ext) {
            Some(MediaType::Image) => Self::extract_image_dimensions(path),
            Some(MediaType::Video) => Self::extract_video_dimensions(path),
            Some(MediaType::Folder) | None => {
                warn!("Unknown media type for extension: {}", ext);
                Ok((ERROR_DIMENSION, ERROR_DIMENSION))
            }
        }
    }

    /// Extracts full metadata including duration for videos.
    pub fn extract_metadata(path: &Path) -> Result<MediaMetadata> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        match MediaType::from_extension(&ext) {
            Some(MediaType::Image) => {
                let (width, height) = Self::extract_image_dimensions(path)?;
                Ok(MediaMetadata {
                    width,
                    height,
                    duration_ms: None,
                    has_warnings: width == ERROR_DIMENSION,
                })
            }
            Some(MediaType::Video) => Self::extract_video_metadata(path),
            Some(MediaType::Folder) | None => {
                warn!("Unknown media type for extension: {}", ext);
                Ok(MediaMetadata {
                    width: ERROR_DIMENSION,
                    height: ERROR_DIMENSION,
                    duration_ms: None,
                    has_warnings: true,
                })
            }
        }
    }

    /// Extracts image dimensions by reading only the header when possible.
    ///
    /// Uses the `image` crate's dimension reading which is optimized to
    /// read minimal data for most formats.
    fn extract_image_dimensions(path: &Path) -> Result<(u32, u32)> {
        trace!("Extracting image dimensions from {:?}", path);

        // Try to read dimensions without decoding the full image
        match ImageReader::open(path) {
            Ok(reader) => match reader.into_dimensions() {
                Ok((width, height)) => {
                    trace!("Got dimensions {}x{} for {:?}", width, height, path);
                    Ok((width, height))
                }
                Err(e) => {
                    warn!("Failed to read image dimensions for {:?}: {}", path, e);
                    Ok((ERROR_DIMENSION, ERROR_DIMENSION))
                }
            },
            Err(e) => {
                warn!("Failed to open image {:?}: {}", path, e);
                Ok((ERROR_DIMENSION, ERROR_DIMENSION))
            }
        }
    }

    /// Extracts video dimensions using basic container parsing.
    ///
    /// This is a simplified implementation that handles common formats.
    /// For full video support, consider using ffprobe or similar.
    fn extract_video_dimensions(path: &Path) -> Result<(u32, u32)> {
        trace!("Extracting video dimensions from {:?}", path);

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        // Try format-specific parsing
        let result = match ext.as_str() {
            "webm" | "mkv" => Self::parse_matroska_dimensions(path),
            "mp4" | "mov" => Self::parse_mp4_dimensions(path),
            "avi" => Self::parse_avi_dimensions(path),
            _ => {
                debug!("No specific parser for video format: {}", ext);
                Ok((ERROR_DIMENSION, ERROR_DIMENSION))
            }
        };

        match result {
            Ok((w, h)) if w > 0 && h > 0 => Ok((w, h)),
            Ok(_) => {
                debug!(
                    "Could not parse video dimensions for {:?}, using fallback",
                    path
                );
                // Fallback: use common HD dimensions as placeholder
                // The UI can update this when the video is actually played
                Ok((ERROR_DIMENSION, ERROR_DIMENSION))
            }
            Err(e) => {
                warn!("Error parsing video {:?}: {}", path, e);
                Ok((ERROR_DIMENSION, ERROR_DIMENSION))
            }
        }
    }

    /// Extracts full video metadata including duration.
    fn extract_video_metadata(path: &Path) -> Result<MediaMetadata> {
        let (width, height) = Self::extract_video_dimensions(path)?;

        // Duration extraction is format-dependent and complex
        // For now, return None and let the video player provide it
        Ok(MediaMetadata {
            width,
            height,
            duration_ms: None,
            has_warnings: width == ERROR_DIMENSION,
        })
    }

    /// Parses Matroska/WebM container for video dimensions.
    ///
    /// WebM/MKV use EBML encoding. This is a simplified parser that looks
    /// for the video track's PixelWidth and PixelHeight elements.
    fn parse_matroska_dimensions(path: &Path) -> Result<(u32, u32)> {
        let file = File::open(path).context("Failed to open video file")?;
        let mut reader = BufReader::new(file);

        // Read first 64KB which should contain the track info
        let mut buffer = vec![0u8; 65536];
        let bytes_read = reader.read(&mut buffer)?;
        let buffer = &buffer[..bytes_read];

        // EBML element IDs for video dimensions
        // PixelWidth: 0xB0
        // PixelHeight: 0xBA
        // These appear within the Video track entry

        let mut width = 0u32;
        let mut height = 0u32;

        // Simple pattern search for dimension elements
        // This is not a full EBML parser but works for most files
        let mut i = 0;
        while i < buffer.len().saturating_sub(8) {
            // Look for PixelWidth (element ID 0xB0)
            if buffer[i] == 0xB0 && i + 4 < buffer.len() {
                if let Some(value) = Self::read_ebml_uint(&buffer[i + 1..]) {
                    if value > 0 && value < 65536 {
                        width = value as u32;
                    }
                }
            }
            // Look for PixelHeight (element ID 0xBA)
            if buffer[i] == 0xBA && i + 4 < buffer.len() {
                if let Some(value) = Self::read_ebml_uint(&buffer[i + 1..]) {
                    if value > 0 && value < 65536 {
                        height = value as u32;
                    }
                }
            }

            // Early exit if we found both
            if width > 0 && height > 0 {
                break;
            }

            i += 1;
        }

        trace!("Matroska parsed dimensions: {}x{}", width, height);
        Ok((width, height))
    }

    /// Reads a variable-length EBML unsigned integer.
    fn read_ebml_uint(data: &[u8]) -> Option<u64> {
        if data.is_empty() {
            return None;
        }

        // EBML VINT: leading zeros indicate byte count
        let first = data[0];
        if first == 0 {
            return None;
        }

        let len = first.leading_zeros() as usize + 1;
        if len > 8 || len > data.len() {
            return None;
        }

        // Read the size bytes
        if len > data.len() {
            return None;
        }
        let _size_data = &data[..len];

        // Check there's enough data after size for the value
        let value_start = len;
        if value_start >= data.len() {
            return None;
        }

        // For simple cases, just read the next few bytes as the value
        let remaining = &data[value_start..];
        if remaining.is_empty() {
            return None;
        }

        // Read up to 4 bytes as the dimension value
        let value_len = (remaining.len()).min(4);
        let mut value = 0u64;
        for &byte in &remaining[..value_len] {
            value = (value << 8) | (byte as u64);
        }

        // Sanity check: dimensions should be reasonable
        if value > 0 && value < 65536 {
            Some(value)
        } else {
            None
        }
    }

    /// Parses MP4/MOV container for video dimensions.
    ///
    /// MP4 uses boxes (atoms). We look for the tkhd (track header) box
    /// which contains width and height.
    fn parse_mp4_dimensions(path: &Path) -> Result<(u32, u32)> {
        let file = File::open(path).context("Failed to open video file")?;
        let mut reader = BufReader::new(file);

        // Read first 128KB which should contain moov/trak/tkhd
        let mut buffer = vec![0u8; 131072];
        let bytes_read = reader.read(&mut buffer)?;
        let buffer = &buffer[..bytes_read];

        // Look for 'tkhd' box (track header) which contains dimensions
        // Format: 4 bytes size, 4 bytes 'tkhd', version/flags, ...
        // Width and height are at offset 76 (v0) or 88 (v1) as 16.16 fixed point

        for i in 0..buffer.len().saturating_sub(92) {
            if &buffer[i..i + 4] == b"tkhd" {
                // Found track header
                let version = buffer[i + 4];
                let offset = if version == 0 { i + 76 } else { i + 88 };

                if offset + 8 <= buffer.len() {
                    // Width and height are 4 bytes each, 16.16 fixed point
                    let width_fixed = u32::from_be_bytes([
                        buffer[offset],
                        buffer[offset + 1],
                        buffer[offset + 2],
                        buffer[offset + 3],
                    ]);
                    let height_fixed = u32::from_be_bytes([
                        buffer[offset + 4],
                        buffer[offset + 5],
                        buffer[offset + 6],
                        buffer[offset + 7],
                    ]);

                    // Convert from 16.16 fixed point
                    let width = width_fixed >> 16;
                    let height = height_fixed >> 16;

                    if width > 0 && height > 0 && width < 65536 && height < 65536 {
                        trace!("MP4 parsed dimensions: {}x{}", width, height);
                        return Ok((width, height));
                    }
                }
            }
        }

        // Fallback: look for 'stsd' -> 'avc1' or 'hvc1' which also contain dimensions
        for i in 0..buffer.len().saturating_sub(40) {
            // Check for avc1, hvc1, mp4v video sample entries
            let tag = &buffer[i..i + 4];
            if tag == b"avc1" || tag == b"hvc1" || tag == b"mp4v" || tag == b"vp09" {
                // Width is at offset 24, height at offset 26 (as u16 BE)
                if i + 28 <= buffer.len() {
                    let width = u16::from_be_bytes([buffer[i + 24], buffer[i + 25]]) as u32;
                    let height = u16::from_be_bytes([buffer[i + 26], buffer[i + 27]]) as u32;

                    if width > 0 && height > 0 {
                        trace!("MP4 stsd parsed dimensions: {}x{}", width, height);
                        return Ok((width, height));
                    }
                }
            }
        }

        Ok((ERROR_DIMENSION, ERROR_DIMENSION))
    }

    /// Parses AVI container for video dimensions.
    ///
    /// AVI uses RIFF chunks. The video dimensions are in the BITMAPINFOHEADER
    /// within the 'strf' chunk.
    fn parse_avi_dimensions(path: &Path) -> Result<(u32, u32)> {
        let file = File::open(path).context("Failed to open video file")?;
        let mut reader = BufReader::new(file);

        // Read first 64KB
        let mut buffer = vec![0u8; 65536];
        let bytes_read = reader.read(&mut buffer)?;
        let buffer = &buffer[..bytes_read];

        // Look for 'strf' chunk followed by BITMAPINFOHEADER
        for i in 0..buffer.len().saturating_sub(48) {
            if &buffer[i..i + 4] == b"strf" {
                // Next 4 bytes are chunk size
                // Then BITMAPINFOHEADER starts
                // biWidth at offset 4, biHeight at offset 8 (both i32 LE)
                let header_start = i + 8;
                if header_start + 12 <= buffer.len() {
                    let width = i32::from_le_bytes([
                        buffer[header_start + 4],
                        buffer[header_start + 5],
                        buffer[header_start + 6],
                        buffer[header_start + 7],
                    ]);
                    let height = i32::from_le_bytes([
                        buffer[header_start + 8],
                        buffer[header_start + 9],
                        buffer[header_start + 10],
                        buffer[header_start + 11],
                    ]);

                    // Height can be negative (top-down bitmap)
                    let height = height.unsigned_abs();
                    let width = width.unsigned_abs();

                    if width > 0 && height > 0 && width < 65536 && height < 65536 {
                        trace!("AVI parsed dimensions: {}x{}", width, height);
                        return Ok((width, height));
                    }
                }
            }
        }

        Ok((ERROR_DIMENSION, ERROR_DIMENSION))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_error_dimension_constant() {
        assert_eq!(ERROR_DIMENSION, 0);
    }

    #[test]
    fn test_unknown_extension() {
        let path = Path::new("/fake/file.xyz");
        let result = MetadataExtractor::extract_dimensions(path);
        // Should return error dimensions without panicking
        assert!(result.is_ok());
        let (w, h) = result.unwrap();
        assert_eq!(w, ERROR_DIMENSION);
        assert_eq!(h, ERROR_DIMENSION);
    }

    #[test]
    fn test_nonexistent_image() {
        let path = Path::new("/nonexistent/image.jpg");
        let result = MetadataExtractor::extract_dimensions(path);
        // Should handle gracefully
        assert!(result.is_ok());
        let (w, h) = result.unwrap();
        assert_eq!(w, ERROR_DIMENSION);
        assert_eq!(h, ERROR_DIMENSION);
    }

    #[test]
    fn test_corrupt_image_data() {
        // Create a file with invalid image data
        let mut temp = NamedTempFile::with_suffix(".jpg").unwrap();
        temp.write_all(b"not a real jpeg file").unwrap();

        let result = MetadataExtractor::extract_dimensions(temp.path());
        // Should handle gracefully without panicking
        assert!(result.is_ok());
    }

    #[test]
    fn test_metadata_extraction_wrapper() {
        let path = Path::new("/fake/video.mp4");
        let result = MetadataExtractor::extract_metadata(path);
        assert!(result.is_ok());
        let meta = result.unwrap();
        assert!(meta.has_warnings); // Should have warnings for non-existent file
    }
}
