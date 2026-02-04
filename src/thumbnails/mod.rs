//! Thumbnail pipeline for the idxd media browser.
//!
//! This module provides:
//! - `ThumbnailGenerator` - Generates thumbnails from source images
//! - `ThumbnailCache` - Disk and memory caching with LRU eviction
//! - `ThumbnailQueue` - Worker queue for async generation

pub mod cache;
pub mod generator;
pub mod queue;
