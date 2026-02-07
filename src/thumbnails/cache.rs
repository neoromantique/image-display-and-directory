//! Thumbnail caching with both disk and memory layers.
//!
//! - Disk cache: Stores thumbnails in XDG_CACHE_HOME/idxd/thumbs/
//! - Memory cache: LRU cache of GdkTexture with configurable size limit
//!
//! Filenames are based on xxhash of (path + mtime + size) for fast invalidation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use gdk4::prelude::TextureExt;
use gdk4::Texture;
use lru::LruCache;
use parking_lot::RwLock;
use tracing::{debug, trace, warn};
use xxhash_rust::xxh3::xxh3_64;

use super::generator::{ThumbnailGenerator, DEFAULT_THUMB_HEIGHT};

/// Default memory cache size in megabytes.
const DEFAULT_MAX_MEMORY_MB: usize = 192;

/// Minimum memory cache size in megabytes.
const MIN_MEMORY_MB: usize = 64;

/// Maximum memory cache size in megabytes.
const MAX_MEMORY_MB: usize = 512;

/// Estimated bytes per pixel for RGBA textures.
const BYTES_PER_PIXEL: usize = 4;
/// Bump when thumbnail generation semantics change (e.g., EXIF orientation handling).
const THUMB_CACHE_VERSION: u8 = 2;

/// Default capacity for the LRU cache (number of entries).
const DEFAULT_LRU_CAPACITY: usize = 2048;

/// A cached thumbnail entry containing the texture and metadata.
#[derive(Clone)]
pub struct CachedThumbnail {
    /// The GTK texture ready for display.
    pub texture: Texture,
    /// Width of the thumbnail in pixels.
    pub width: u32,
    /// Height of the thumbnail in pixels.
    pub height: u32,
    /// Estimated memory usage in bytes.
    pub memory_bytes: usize,
}

impl CachedThumbnail {
    fn new(texture: Texture, width: u32, height: u32) -> Self {
        let memory_bytes = (width as usize) * (height as usize) * BYTES_PER_PIXEL;
        Self {
            texture,
            width,
            height,
            memory_bytes,
        }
    }
}

/// Cache key for thumbnail lookups.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// Hash of path + mtime + size.
    hash: u64,
    /// Original path for debugging.
    #[cfg(debug_assertions)]
    path: PathBuf,
}

impl CacheKey {
    /// Create a new cache key from file metadata.
    pub fn new(path: &Path, mtime: i64, size: i64) -> Self {
        let hash = Self::compute_hash(path, mtime, size);
        Self {
            hash,
            #[cfg(debug_assertions)]
            path: path.to_path_buf(),
        }
    }

    /// Compute the xxhash of the key components.
    fn compute_hash(path: &Path, mtime: i64, size: i64) -> u64 {
        // Combine path, mtime, and size into a single buffer for hashing
        let path_str = path.to_string_lossy();
        let mut data = Vec::with_capacity(path_str.len() + 17);
        data.push(THUMB_CACHE_VERSION);
        data.extend_from_slice(path_str.as_bytes());
        data.extend_from_slice(&mtime.to_le_bytes());
        data.extend_from_slice(&size.to_le_bytes());
        xxh3_64(&data)
    }

    /// Get the filename for disk cache storage.
    pub fn disk_filename(&self) -> String {
        format!("{:016x}.jpg", self.hash)
    }
}

/// Thumbnail cache with disk and memory layers.
pub struct ThumbnailCache {
    /// Directory for disk cache storage.
    cache_dir: PathBuf,
    /// Maximum memory usage in bytes.
    max_memory_bytes: usize,
    /// Current memory usage in bytes.
    current_memory_bytes: Arc<RwLock<usize>>,
    /// LRU memory cache of textures.
    memory_cache: Arc<RwLock<LruCache<u64, CachedThumbnail>>>,
    /// Target thumbnail height.
    thumb_height: u32,
}

impl ThumbnailCache {
    /// Create a new thumbnail cache with the specified directory and memory limit.
    pub fn new(cache_dir: PathBuf, max_memory_mb: usize) -> Self {
        let max_memory_mb = max_memory_mb.clamp(MIN_MEMORY_MB, MAX_MEMORY_MB);
        let max_memory_bytes = max_memory_mb * 1024 * 1024;

        // Create the cache directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            warn!(?cache_dir, error = ?e, "Failed to create cache directory");
        }

        debug!(?cache_dir, max_memory_mb, "Initialized thumbnail cache");

        Self {
            cache_dir,
            max_memory_bytes,
            current_memory_bytes: Arc::new(RwLock::new(0)),
            memory_cache: Arc::new(RwLock::new(LruCache::new(
                std::num::NonZeroUsize::new(DEFAULT_LRU_CAPACITY).unwrap(),
            ))),
            thumb_height: DEFAULT_THUMB_HEIGHT,
        }
    }

    /// Create a new thumbnail cache using the default XDG cache directory.
    pub fn new_default(max_memory_mb: usize) -> Result<Self> {
        let cache_dir = Self::default_cache_dir()?;
        Ok(Self::new(cache_dir, max_memory_mb))
    }

    /// Get the default cache directory path.
    pub fn default_cache_dir() -> Result<PathBuf> {
        let proj_dirs =
            ProjectDirs::from("", "", "idxd").context("Failed to determine project directories")?;
        Ok(proj_dirs.cache_dir().join("thumbs"))
    }

    /// Set the target thumbnail height.
    pub fn set_thumb_height(&mut self, height: u32) {
        self.thumb_height = height;
    }

    /// Get a thumbnail from the cache, generating it if necessary.
    ///
    /// This is the main entry point for retrieving thumbnails.
    /// It checks the memory cache first, then disk cache, and generates if needed.
    pub fn get_or_generate(&self, path: &Path, mtime: i64, size: i64) -> Result<CachedThumbnail> {
        let key = CacheKey::new(path, mtime, size);

        // Try memory cache first
        if let Some(cached) = self.get_from_memory(&key) {
            trace!(?path, "Memory cache hit");
            return Ok(cached);
        }

        // Try disk cache
        let disk_path = self.disk_path(&key);
        if disk_path.exists() {
            if let Ok(cached) = self.load_from_disk(&key, &disk_path) {
                trace!(?path, "Disk cache hit");
                return Ok(cached);
            }
            // If disk load failed, remove corrupted file
            let _ = std::fs::remove_file(&disk_path);
        }

        // Generate new thumbnail
        debug!(?path, "Cache miss, generating thumbnail");
        self.generate_and_cache(path, &key)
    }

    /// Check if a thumbnail exists in cache (memory or disk).
    pub fn exists(&self, path: &Path, mtime: i64, size: i64) -> bool {
        let key = CacheKey::new(path, mtime, size);

        // Check memory cache
        if self.memory_cache.read().contains(&key.hash) {
            return true;
        }

        // Check disk cache
        self.disk_path(&key).exists()
    }

    /// Get a thumbnail from memory cache only.
    pub fn get_from_memory(&self, key: &CacheKey) -> Option<CachedThumbnail> {
        self.memory_cache.write().get(&key.hash).cloned()
    }

    /// Get the disk path for a cache key.
    pub fn disk_path(&self, key: &CacheKey) -> PathBuf {
        self.cache_dir.join(key.disk_filename())
    }

    /// Load a thumbnail from disk and add to memory cache.
    fn load_from_disk(&self, key: &CacheKey, disk_path: &Path) -> Result<CachedThumbnail> {
        let texture = Texture::from_filename(disk_path)
            .map_err(|e| anyhow::anyhow!("Failed to load texture: {}", e))?;

        let width = texture.width() as u32;
        let height = texture.height() as u32;
        let cached = CachedThumbnail::new(texture, width, height);

        // Add to memory cache
        self.add_to_memory_cache(key.hash, cached.clone());

        Ok(cached)
    }

    /// Generate a new thumbnail and cache it.
    fn generate_and_cache(&self, path: &Path, key: &CacheKey) -> Result<CachedThumbnail> {
        let disk_path = self.disk_path(key);

        // Generate the thumbnail
        let (width, height) = ThumbnailGenerator::generate(path, &disk_path, self.thumb_height)?;

        // Load as texture
        let texture = Texture::from_filename(&disk_path)
            .map_err(|e| anyhow::anyhow!("Failed to load generated thumbnail: {}", e))?;

        let cached = CachedThumbnail::new(texture, width, height);

        // Add to memory cache
        self.add_to_memory_cache(key.hash, cached.clone());

        Ok(cached)
    }

    /// Add an entry to the memory cache, evicting if necessary.
    fn add_to_memory_cache(&self, hash: u64, cached: CachedThumbnail) {
        let new_size = cached.memory_bytes;

        // Evict entries if we're over the memory limit
        self.evict_if_needed(new_size);

        // Add to cache
        let mut cache = self.memory_cache.write();
        if let Some(old) = cache.put(hash, cached) {
            // Subtract old entry's size
            let mut current = self.current_memory_bytes.write();
            *current = current.saturating_sub(old.memory_bytes);
        }

        // Add new entry's size
        *self.current_memory_bytes.write() += new_size;
    }

    /// Evict entries from the memory cache until we have room for `needed_bytes`.
    fn evict_if_needed(&self, needed_bytes: usize) {
        let mut current = self.current_memory_bytes.write();

        if *current + needed_bytes <= self.max_memory_bytes {
            return;
        }

        let mut cache = self.memory_cache.write();

        // Evict least recently used entries
        while *current + needed_bytes > self.max_memory_bytes {
            if let Some((_, evicted)) = cache.pop_lru() {
                *current = current.saturating_sub(evicted.memory_bytes);
                trace!(
                    evicted_bytes = evicted.memory_bytes,
                    current_bytes = *current,
                    "Evicted thumbnail from memory cache"
                );
            } else {
                // Cache is empty
                break;
            }
        }
    }

    /// Clear the memory cache.
    pub fn clear_memory(&self) {
        self.memory_cache.write().clear();
        *self.current_memory_bytes.write() = 0;
        debug!("Cleared memory cache");
    }

    /// Clear both memory and disk caches.
    pub fn clear_all(&self) -> Result<()> {
        self.clear_memory();

        // Remove all files in the cache directory
        if self.cache_dir.exists() {
            for entry in std::fs::read_dir(&self.cache_dir)? {
                if let Ok(entry) = entry {
                    let path = entry.path();
                    if path.is_file() && path.extension().map_or(false, |e| e == "jpg") {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }

        debug!(?self.cache_dir, "Cleared disk cache");
        Ok(())
    }

    /// Get the current memory usage in bytes.
    pub fn memory_usage(&self) -> usize {
        *self.current_memory_bytes.read()
    }

    /// Get the number of entries in the memory cache.
    pub fn memory_entry_count(&self) -> usize {
        self.memory_cache.read().len()
    }

    /// Get the maximum memory limit in bytes.
    pub fn max_memory(&self) -> usize {
        self.max_memory_bytes
    }

    /// Get the cache directory path.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Preload a thumbnail into memory cache from disk (if it exists).
    /// Returns true if the thumbnail was loaded, false otherwise.
    pub fn preload(&self, path: &Path, mtime: i64, size: i64) -> bool {
        let key = CacheKey::new(path, mtime, size);

        // Already in memory?
        if self.memory_cache.read().contains(&key.hash) {
            return true;
        }

        // Try to load from disk
        let disk_path = self.disk_path(&key);
        if disk_path.exists() {
            self.load_from_disk(&key, &disk_path).is_ok()
        } else {
            false
        }
    }

    /// Remove a specific thumbnail from cache.
    pub fn remove(&self, path: &Path, mtime: i64, size: i64) -> bool {
        let key = CacheKey::new(path, mtime, size);

        // Remove from memory
        let memory_removed = if let Some(evicted) = self.memory_cache.write().pop(&key.hash) {
            *self.current_memory_bytes.write() -= evicted.memory_bytes;
            true
        } else {
            false
        };

        // Remove from disk
        let disk_path = self.disk_path(&key);
        let disk_removed = if disk_path.exists() {
            std::fs::remove_file(&disk_path).is_ok()
        } else {
            false
        };

        memory_removed || disk_removed
    }
}

impl Clone for ThumbnailCache {
    fn clone(&self) -> Self {
        Self {
            cache_dir: self.cache_dir.clone(),
            max_memory_bytes: self.max_memory_bytes,
            current_memory_bytes: Arc::clone(&self.current_memory_bytes),
            memory_cache: Arc::clone(&self.memory_cache),
            thumb_height: self.thumb_height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_hash() {
        let key1 = CacheKey::new(Path::new("/test/image.jpg"), 1234567890, 1024);
        let key2 = CacheKey::new(Path::new("/test/image.jpg"), 1234567890, 1024);
        let key3 = CacheKey::new(Path::new("/test/image.jpg"), 1234567891, 1024);

        // Same inputs should produce same hash
        assert_eq!(key1.hash, key2.hash);
        // Different mtime should produce different hash
        assert_ne!(key1.hash, key3.hash);
    }

    #[test]
    fn test_disk_filename() {
        let key = CacheKey::new(Path::new("/test/image.jpg"), 1234567890, 1024);
        let filename = key.disk_filename();

        // Should be a 16-character hex string with .jpg extension
        assert!(filename.ends_with(".jpg"));
        assert_eq!(filename.len(), 20); // 16 hex + ".jpg"
    }

    #[test]
    fn test_memory_limit_clamping() {
        let temp_dir = std::env::temp_dir().join("idxd_test_cache");
        let _ = std::fs::create_dir_all(&temp_dir);

        // Test minimum clamping
        let cache = ThumbnailCache::new(temp_dir.clone(), 10);
        assert_eq!(cache.max_memory(), MIN_MEMORY_MB * 1024 * 1024);

        // Test maximum clamping
        let cache = ThumbnailCache::new(temp_dir.clone(), 1000);
        assert_eq!(cache.max_memory(), MAX_MEMORY_MB * 1024 * 1024);

        // Test normal value
        let cache = ThumbnailCache::new(temp_dir, 200);
        assert_eq!(cache.max_memory(), 200 * 1024 * 1024);
    }
}
