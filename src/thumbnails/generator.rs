//! Thumbnail generation using the image crate.
//!
//! Generates thumbnails at approximately 256px height while preserving aspect ratio.
//! Stores exact generated dimensions to avoid re-scaling in the UI.

use std::path::Path;

use anyhow::{Context, Result};
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageFormat};
use tracing::debug;

/// Default target height for thumbnails in pixels.
pub const DEFAULT_THUMB_HEIGHT: u32 = 256;

/// Minimum width for thumbnails (to handle extreme aspect ratios).
const MIN_THUMB_WIDTH: u32 = 64;

/// Maximum width for thumbnails (to handle extreme panoramas).
const MAX_THUMB_WIDTH: u32 = 1024;

/// JPEG quality for thumbnail encoding (0-100).
const JPEG_QUALITY: u8 = 85;

/// Thumbnail generator that creates resized images for caching.
pub struct ThumbnailGenerator;

/// Result of thumbnail generation containing dimensions.
#[derive(Debug, Clone, Copy)]
pub struct ThumbnailResult {
    pub width: u32,
    pub height: u32,
}

impl ThumbnailGenerator {
    /// Generate a thumbnail from the source image and save it to the destination path.
    ///
    /// # Arguments
    /// * `src` - Source image path
    /// * `dst` - Destination path for the thumbnail
    /// * `target_height` - Target height in pixels (width is calculated to preserve aspect ratio)
    ///
    /// # Returns
    /// The actual dimensions (width, height) of the generated thumbnail.
    pub fn generate(src: &Path, dst: &Path, target_height: u32) -> Result<(u32, u32)> {
        let result = Self::generate_thumbnail(src, dst, target_height)?;
        Ok((result.width, result.height))
    }

    /// Generate a thumbnail with full result information.
    pub fn generate_thumbnail(
        src: &Path,
        dst: &Path,
        target_height: u32,
    ) -> Result<ThumbnailResult> {
        debug!(?src, ?dst, target_height, "Generating thumbnail");

        // Load the source image
        let img = Self::load_image(src)?;
        let (src_width, src_height) = img.dimensions();

        // Calculate target dimensions preserving aspect ratio
        let (thumb_width, thumb_height) =
            Self::calculate_dimensions(src_width, src_height, target_height);

        debug!(
            src_width,
            src_height, thumb_width, thumb_height, "Calculated thumbnail dimensions"
        );

        // Resize the image using a high-quality filter
        // CatmullRom provides good quality/speed balance for downscaling
        let thumbnail = img.resize_exact(thumb_width, thumb_height, FilterType::CatmullRom);

        // Ensure the parent directory exists
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create thumbnail directory: {:?}", parent))?;
        }

        // Save the thumbnail
        Self::save_thumbnail(&thumbnail, dst)?;

        Ok(ThumbnailResult {
            width: thumb_width,
            height: thumb_height,
        })
    }

    /// Load an image from disk, handling various formats.
    fn load_image(path: &Path) -> Result<DynamicImage> {
        // Use image::open which auto-detects format
        let img = image::open(path).with_context(|| format!("Failed to load image: {:?}", path))?;

        Ok(img)
    }

    /// Determine image format from file extension.
    fn format_from_extension(path: &Path) -> Option<ImageFormat> {
        let ext = path.extension()?.to_str()?.to_lowercase();
        match ext.as_str() {
            "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
            "png" => Some(ImageFormat::Png),
            "webp" => Some(ImageFormat::WebP),
            "gif" => Some(ImageFormat::Gif),
            "bmp" => Some(ImageFormat::Bmp),
            "tiff" | "tif" => Some(ImageFormat::Tiff),
            _ => None,
        }
    }

    /// Calculate thumbnail dimensions preserving aspect ratio.
    ///
    /// The target height is used as the base, with width calculated proportionally.
    /// Width is clamped to MIN_THUMB_WIDTH..MAX_THUMB_WIDTH to handle extreme aspect ratios.
    fn calculate_dimensions(src_width: u32, src_height: u32, target_height: u32) -> (u32, u32) {
        if src_height == 0 || src_width == 0 {
            return (target_height, target_height);
        }

        // If source is smaller than target, don't upscale
        let effective_height = target_height.min(src_height);

        // Calculate width preserving aspect ratio
        let aspect_ratio = src_width as f64 / src_height as f64;
        let calculated_width = (effective_height as f64 * aspect_ratio).round() as u32;

        // Clamp width to reasonable bounds
        let final_width = calculated_width.clamp(MIN_THUMB_WIDTH, MAX_THUMB_WIDTH);

        // If width was clamped, recalculate height to maintain aspect ratio
        let final_height = if final_width != calculated_width {
            (final_width as f64 / aspect_ratio).round() as u32
        } else {
            effective_height
        };

        (final_width.max(1), final_height.max(1))
    }

    /// Save thumbnail to disk as JPEG for optimal size/quality balance.
    fn save_thumbnail(img: &DynamicImage, dst: &Path) -> Result<()> {
        use image::codecs::jpeg::JpegEncoder;
        use std::fs::File;
        use std::io::BufWriter;

        let file = File::create(dst)
            .with_context(|| format!("Failed to create thumbnail file: {:?}", dst))?;

        let mut writer = BufWriter::new(file);

        // Convert to RGB8 for JPEG (no alpha channel)
        let rgb_img = img.to_rgb8();

        let encoder = JpegEncoder::new_with_quality(&mut writer, JPEG_QUALITY);
        rgb_img
            .write_with_encoder(encoder)
            .with_context(|| format!("Failed to encode thumbnail: {:?}", dst))?;

        debug!(?dst, "Saved thumbnail");
        Ok(())
    }

    /// Generate a thumbnail and return the image data without saving to disk.
    /// Useful for in-memory processing or when the caller wants to handle storage.
    pub fn generate_in_memory(src: &Path, target_height: u32) -> Result<(Vec<u8>, u32, u32)> {
        let img = Self::load_image(src)?;
        let (src_width, src_height) = img.dimensions();

        let (thumb_width, thumb_height) =
            Self::calculate_dimensions(src_width, src_height, target_height);

        let thumbnail = img.resize_exact(thumb_width, thumb_height, FilterType::CatmullRom);

        // Encode to JPEG in memory
        let rgb_img = thumbnail.to_rgb8();
        let mut buffer = Vec::new();

        {
            use image::codecs::jpeg::JpegEncoder;
            let encoder = JpegEncoder::new_with_quality(&mut buffer, JPEG_QUALITY);
            rgb_img
                .write_with_encoder(encoder)
                .context("Failed to encode thumbnail to memory")?;
        }

        Ok((buffer, thumb_width, thumb_height))
    }

    /// Check if a source file can be processed as an image.
    pub fn can_generate(path: &Path) -> bool {
        Self::format_from_extension(path).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_dimensions_normal() {
        // 1920x1080 -> target 256 height
        let (w, h) = ThumbnailGenerator::calculate_dimensions(1920, 1080, 256);
        assert_eq!(h, 256);
        // Width should be proportional: 1920 * (256/1080) = 455
        assert!((w as i32 - 455).abs() <= 1);
    }

    #[test]
    fn test_calculate_dimensions_small_source() {
        // Source smaller than target - don't upscale
        let (w, h) = ThumbnailGenerator::calculate_dimensions(200, 100, 256);
        assert_eq!(h, 100);
        assert_eq!(w, 200);
    }

    #[test]
    fn test_calculate_dimensions_extreme_panorama() {
        // Very wide image - width should be clamped
        let (w, h) = ThumbnailGenerator::calculate_dimensions(10000, 500, 256);
        assert_eq!(w, MAX_THUMB_WIDTH);
        // Height recalculated: 1024 / (10000/500) = 51
        assert!(h < 256);
    }

    #[test]
    fn test_calculate_dimensions_extreme_portrait() {
        // Very tall image - width should be clamped to minimum
        let (w, h) = ThumbnailGenerator::calculate_dimensions(100, 5000, 256);
        assert_eq!(w, MIN_THUMB_WIDTH);
    }

    #[test]
    fn test_format_from_extension() {
        assert_eq!(
            ThumbnailGenerator::format_from_extension(Path::new("test.jpg")),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(
            ThumbnailGenerator::format_from_extension(Path::new("test.PNG")),
            Some(ImageFormat::Png)
        );
        assert_eq!(
            ThumbnailGenerator::format_from_extension(Path::new("test.txt")),
            None
        );
    }
}
