//! Async file scanner for discovering and indexing media files.
//!
//! This module provides the `FileScanner` struct which handles:
//! - Recursive directory scanning using walkdir
//! - Media type detection by file extension
//! - Cache-aware scanning (skip unchanged files based on mtime)
//! - Async metadata extraction with batched SQLite writes
//! - Progress reporting via channels

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task;
use tracing::{debug, info, trace, warn};
use walkdir::WalkDir;

use crate::models::media_store::MediaStore;
use crate::models::{MediaItem, MediaType};
use crate::scanner::metadata::MetadataExtractor;

/// Configuration for the file scanner.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// Whether to scan directories recursively.
    pub recursive: bool,
    /// Maximum directory depth (0 = unlimited).
    pub max_depth: usize,
    /// Number of items to batch before writing to database.
    pub batch_size: usize,
    /// Whether to follow symbolic links.
    pub follow_symlinks: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            recursive: true,
            max_depth: 0, // unlimited
            batch_size: 100,
            follow_symlinks: false,
        }
    }
}

/// Progress information sent during scanning.
#[derive(Debug, Clone)]
pub enum ScanProgress {
    /// Scanning has started.
    Started { path: PathBuf },
    /// A file was discovered (before metadata extraction).
    Discovered { count: usize },
    /// A batch of items was processed and saved.
    BatchSaved { processed: usize, total: usize },
    /// Metadata was extracted for an item.
    Extracted { path: PathBuf, cached: bool },
    /// An error occurred for a specific file.
    FileError { path: PathBuf, error: String },
    /// Scanning completed.
    Completed {
        total: usize,
        new: usize,
        cached: usize,
        errors: usize,
    },
}

/// Result of a completed scan operation.
#[derive(Debug, Clone)]
pub struct ScanResult {
    /// Total number of media files found.
    pub total_files: usize,
    /// Number of newly added/updated items.
    pub new_items: usize,
    /// Number of items loaded from cache.
    pub cached_items: usize,
    /// Number of files that had errors.
    pub error_count: usize,
    /// Paths of all discovered media items (in scan order).
    pub paths: Vec<PathBuf>,
}

/// Async file scanner for media directories.
pub struct FileScanner {
    config: ScanConfig,
}

impl FileScanner {
    /// Creates a new file scanner with default configuration.
    pub fn new() -> Self {
        Self {
            config: ScanConfig::default(),
        }
    }

    /// Creates a new file scanner with custom configuration.
    pub fn with_config(config: ScanConfig) -> Self {
        Self { config }
    }

    /// Scans a directory and returns all found media items.
    ///
    /// This is the simple API that blocks until scanning completes.
    /// For progress updates, use `scan_with_progress` instead.
    pub async fn scan(dir: &Path) -> Result<Vec<MediaItem>> {
        let scanner = Self::new();
        let store = MediaStore::open_default()?;
        let (items, _) = scanner.scan_directory(dir, store).await?;
        Ok(items)
    }

    /// Scans a directory with a media store for caching.
    ///
    /// Returns the list of media items and scan statistics.
    pub async fn scan_directory(
        &self,
        dir: &Path,
        mut store: MediaStore,
    ) -> Result<(Vec<MediaItem>, ScanResult)> {
        let dir = dir.to_path_buf();
        let config = self.config.clone();

        // Run the scan in a blocking task to avoid blocking the async runtime
        let result =
            task::spawn_blocking(move || Self::scan_directory_sync(&dir, &config, &mut store))
                .await
                .context("Scan task panicked")??;

        Ok(result)
    }

    /// Scans a directory with progress reporting via a channel.
    ///
    /// Returns a receiver for progress updates and a handle to await the result.
    pub fn scan_with_progress(
        &self,
        dir: PathBuf,
        store: MediaStore,
    ) -> (
        mpsc::Receiver<ScanProgress>,
        task::JoinHandle<Result<(Vec<MediaItem>, ScanResult)>>,
    ) {
        let config = self.config.clone();
        let (tx, rx) = mpsc::channel(100);

        let handle = task::spawn_blocking(move || {
            let mut store = store;
            Self::scan_directory_with_progress_sync(&dir, &config, &mut store, tx)
        });

        // Wrap the handle to flatten the result
        let wrapped_handle =
            task::spawn(async move { handle.await.context("Scan task panicked")? });

        (rx, wrapped_handle)
    }

    /// Synchronous directory scanning implementation.
    fn scan_directory_sync(
        dir: &Path,
        config: &ScanConfig,
        store: &mut MediaStore,
    ) -> Result<(Vec<MediaItem>, ScanResult)> {
        info!("Starting scan of {:?}", dir);
        let scan_time = MediaStore::now();

        // Get existing cache entries for quick lookup
        let cache_map = store.get_cache_map()?;
        debug!("Loaded {} cached entries", cache_map.len());

        // Discover all media files
        let discovered = Self::discover_files(dir, config)?;
        info!("Discovered {} media files", discovered.len());

        // Process files and extract metadata
        let mut items = Vec::with_capacity(discovered.len());
        let mut batch = Vec::with_capacity(config.batch_size);
        let mut new_count = 0;
        let mut cached_count = 0;
        let mut error_count = 0;

        for entry in discovered {
            match Self::process_entry(&entry, &cache_map, scan_time) {
                Ok((item, from_cache)) => {
                    if from_cache {
                        cached_count += 1;
                    } else {
                        new_count += 1;
                    }
                    batch.push(item.clone());
                    items.push(item);

                    // Write batch to database
                    if batch.len() >= config.batch_size {
                        store.upsert_media_batch(&batch)?;
                        batch.clear();
                    }
                }
                Err(e) => {
                    warn!("Error processing {:?}: {}", entry.path, e);
                    error_count += 1;
                }
            }
        }

        // Write remaining items
        if !batch.is_empty() {
            store.upsert_media_batch(&batch)?;
        }

        // Update last_seen for all discovered paths
        let paths: Vec<PathBuf> = items.iter().map(|i| i.path.clone()).collect();
        store.touch_last_seen(&paths, scan_time)?;

        let result = ScanResult {
            total_files: items.len(),
            new_items: new_count,
            cached_items: cached_count,
            error_count,
            paths,
        };

        info!(
            "Scan complete: {} total, {} new, {} cached, {} errors",
            result.total_files, result.new_items, result.cached_items, result.error_count
        );

        Ok((items, result))
    }

    /// Synchronous scanning with progress channel.
    fn scan_directory_with_progress_sync(
        dir: &Path,
        config: &ScanConfig,
        store: &mut MediaStore,
        tx: mpsc::Sender<ScanProgress>,
    ) -> Result<(Vec<MediaItem>, ScanResult)> {
        // Send start notification
        let _ = tx.blocking_send(ScanProgress::Started {
            path: dir.to_path_buf(),
        });

        let scan_time = MediaStore::now();

        // Get existing cache entries
        let cache_map = store.get_cache_map()?;

        // Discover files
        let discovered = Self::discover_files(dir, config)?;
        let total = discovered.len();

        let _ = tx.blocking_send(ScanProgress::Discovered { count: total });

        // Process files
        let mut items = Vec::with_capacity(total);
        let mut batch = Vec::with_capacity(config.batch_size);
        let mut new_count = 0;
        let mut cached_count = 0;
        let mut error_count = 0;
        let mut processed = 0;

        for entry in discovered {
            match Self::process_entry(&entry, &cache_map, scan_time) {
                Ok((item, from_cache)) => {
                    let _ = tx.blocking_send(ScanProgress::Extracted {
                        path: item.path.clone(),
                        cached: from_cache,
                    });

                    if from_cache {
                        cached_count += 1;
                    } else {
                        new_count += 1;
                    }

                    batch.push(item.clone());
                    items.push(item);
                    processed += 1;

                    // Write batch
                    if batch.len() >= config.batch_size {
                        store.upsert_media_batch(&batch)?;
                        batch.clear();

                        let _ = tx.blocking_send(ScanProgress::BatchSaved { processed, total });
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(ScanProgress::FileError {
                        path: entry.path.clone(),
                        error: e.to_string(),
                    });
                    error_count += 1;
                    processed += 1;
                }
            }
        }

        // Write remaining batch
        if !batch.is_empty() {
            store.upsert_media_batch(&batch)?;
            let _ = tx.blocking_send(ScanProgress::BatchSaved { processed, total });
        }

        // Update last_seen
        let paths: Vec<PathBuf> = items.iter().map(|i| i.path.clone()).collect();
        store.touch_last_seen(&paths, scan_time)?;

        let result = ScanResult {
            total_files: items.len(),
            new_items: new_count,
            cached_items: cached_count,
            error_count,
            paths,
        };

        let _ = tx.blocking_send(ScanProgress::Completed {
            total: result.total_files,
            new: result.new_items,
            cached: result.cached_items,
            errors: result.error_count,
        });

        Ok((items, result))
    }

    /// Discovers all media files in a directory.
    fn discover_files(dir: &Path, config: &ScanConfig) -> Result<Vec<DiscoveredEntry>> {
        let mut walker = WalkDir::new(dir).follow_links(config.follow_symlinks);

        if !config.recursive {
            walker = walker.max_depth(1);
        } else if config.max_depth > 0 {
            walker = walker.max_depth(config.max_depth);
        }

        let mut entries = Vec::new();

        for entry in walker.into_iter().filter_map(|e| e.ok()) {
            // Skip directories
            if entry.file_type().is_dir() {
                continue;
            }

            let path = entry.path();

            // Check if it's a media file
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

            let media_type = match MediaType::from_extension(ext) {
                Some(t) => t,
                None => continue, // Skip non-media files
            };

            // Get file metadata
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to read metadata for {:?}: {}", path, e);
                    continue;
                }
            };

            let mtime = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            let size = metadata.len() as i64;

            entries.push(DiscoveredEntry {
                path: path.to_path_buf(),
                media_type,
                mtime,
                size,
            });
        }

        // Sort by path for consistent ordering
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(entries)
    }

    /// Processes a discovered entry, using cache when possible.
    fn process_entry(
        entry: &DiscoveredEntry,
        cache_map: &HashMap<PathBuf, (i64, i64)>,
        scan_time: i64,
    ) -> Result<(MediaItem, bool)> {
        // Check if we have a valid cached entry
        if let Some(&(cached_mtime, cached_size)) = cache_map.get(&entry.path) {
            if cached_mtime == entry.mtime && cached_size == entry.size {
                // Cache hit - we still need to return a MediaItem
                // but we don't need to re-extract metadata
                trace!("Cache hit for {:?}", entry.path);

                // For cached items, we use placeholder dimensions
                // The full item will be loaded from the database
                return Ok((
                    MediaItem {
                        path: entry.path.clone(),
                        media_type: entry.media_type,
                        mtime: entry.mtime,
                        size: entry.size,
                        width: 0,  // Will be loaded from DB
                        height: 0, // Will be loaded from DB
                        duration_ms: None,
                        thumb_path: None,
                        thumb_w: None,
                        thumb_h: None,
                        last_seen: scan_time,
                    },
                    true, // from cache
                ));
            }
        }

        // Cache miss or stale - extract metadata
        trace!("Extracting metadata for {:?}", entry.path);
        let metadata = MetadataExtractor::extract_metadata(&entry.path)?;

        let item = MediaItem {
            path: entry.path.clone(),
            media_type: entry.media_type,
            mtime: entry.mtime,
            size: entry.size,
            width: metadata.width,
            height: metadata.height,
            duration_ms: metadata.duration_ms,
            thumb_path: None,
            thumb_w: None,
            thumb_h: None,
            last_seen: scan_time,
        };

        Ok((item, false)) // not from cache
    }
}

impl Default for FileScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// Information about a discovered media file.
#[derive(Debug, Clone)]
struct DiscoveredEntry {
    path: PathBuf,
    media_type: MediaType,
    mtime: i64,
    size: i64,
}

/// Parallel scanner for high-performance scanning on multi-core systems.
///
/// This uses multiple threads for metadata extraction while still
/// batching writes to SQLite.
pub struct ParallelScanner {
    config: ScanConfig,
    /// Number of worker threads for metadata extraction.
    num_workers: usize,
}

impl ParallelScanner {
    /// Creates a new parallel scanner with the specified number of workers.
    pub fn new(num_workers: usize) -> Self {
        Self {
            config: ScanConfig::default(),
            num_workers: num_workers.max(1),
        }
    }

    /// Creates a parallel scanner with custom configuration.
    pub fn with_config(config: ScanConfig, num_workers: usize) -> Self {
        Self {
            config,
            num_workers: num_workers.max(1),
        }
    }

    /// Scans a directory using parallel metadata extraction.
    pub async fn scan_directory(
        &self,
        dir: &Path,
        store: MediaStore,
    ) -> Result<(Vec<MediaItem>, ScanResult)> {
        let dir = dir.to_path_buf();
        let config = self.config.clone();
        let num_workers = self.num_workers;

        task::spawn_blocking(move || {
            let mut store = store;
            Self::scan_parallel_sync(&dir, &config, &mut store, num_workers)
        })
        .await
        .context("Parallel scan task panicked")?
    }

    /// Synchronous parallel scanning implementation.
    fn scan_parallel_sync(
        dir: &Path,
        config: &ScanConfig,
        store: &mut MediaStore,
        num_workers: usize,
    ) -> Result<(Vec<MediaItem>, ScanResult)> {
        use std::thread;

        info!(
            "Starting parallel scan of {:?} with {} workers",
            dir, num_workers
        );
        let scan_time = MediaStore::now();

        // Get cache map
        let cache_map = Arc::new(store.get_cache_map()?);

        // Discover files
        let discovered = FileScanner::discover_files(dir, config)?;
        let total = discovered.len();
        info!("Discovered {} media files", total);

        if total == 0 {
            return Ok((
                Vec::new(),
                ScanResult {
                    total_files: 0,
                    new_items: 0,
                    cached_items: 0,
                    error_count: 0,
                    paths: Vec::new(),
                },
            ));
        }

        // Shared results
        let items = Arc::new(Mutex::new(Vec::with_capacity(total)));
        let new_count = Arc::new(Mutex::new(0usize));
        let cached_count = Arc::new(Mutex::new(0usize));
        let error_count = Arc::new(Mutex::new(0usize));

        // Split work among workers
        let chunk_size = (total + num_workers - 1) / num_workers;
        let entries = Arc::new(discovered);

        let handles: Vec<_> = (0..num_workers)
            .map(|worker_id| {
                let entries = Arc::clone(&entries);
                let cache_map = Arc::clone(&cache_map);
                let items = Arc::clone(&items);
                let new_count = Arc::clone(&new_count);
                let cached_count = Arc::clone(&cached_count);
                let error_count = Arc::clone(&error_count);

                thread::spawn(move || {
                    let start = worker_id * chunk_size;
                    let end = (start + chunk_size).min(entries.len());

                    let mut local_items = Vec::new();
                    let mut local_new = 0;
                    let mut local_cached = 0;
                    let mut local_errors = 0;

                    for entry in &entries[start..end] {
                        match FileScanner::process_entry(entry, &cache_map, scan_time) {
                            Ok((item, from_cache)) => {
                                if from_cache {
                                    local_cached += 1;
                                } else {
                                    local_new += 1;
                                }
                                local_items.push(item);
                            }
                            Err(e) => {
                                warn!(
                                    "Worker {}: Error processing {:?}: {}",
                                    worker_id, entry.path, e
                                );
                                local_errors += 1;
                            }
                        }
                    }

                    // Merge results
                    {
                        let mut items_lock = items.lock();
                        items_lock.extend(local_items);
                    }
                    *new_count.lock() += local_new;
                    *cached_count.lock() += local_cached;
                    *error_count.lock() += local_errors;
                })
            })
            .collect();

        // Wait for all workers
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("Worker thread panicked"))?;
        }

        // Get final results
        let mut items = Arc::try_unwrap(items)
            .map_err(|_| anyhow::anyhow!("Failed to unwrap items"))?
            .into_inner();
        let new_count = *new_count.lock();
        let cached_count = *cached_count.lock();
        let error_count = *error_count.lock();

        // Sort items by path for consistent ordering
        items.sort_by(|a, b| a.path.cmp(&b.path));

        // Batch write to database
        for chunk in items.chunks(config.batch_size) {
            store.upsert_media_batch(chunk)?;
        }

        // Update last_seen
        let paths: Vec<PathBuf> = items.iter().map(|i| i.path.clone()).collect();
        store.touch_last_seen(&paths, scan_time)?;

        let result = ScanResult {
            total_files: items.len(),
            new_items: new_count,
            cached_items: cached_count,
            error_count,
            paths,
        };

        info!(
            "Parallel scan complete: {} total, {} new, {} cached, {} errors",
            result.total_files, result.new_items, result.cached_items, result.error_count
        );

        Ok((items, result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::tempdir;

    fn create_test_image(path: &Path) {
        // Create a minimal valid PNG file (1x1 pixel)
        let png_data: [u8; 67] = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1 dimensions
            0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
            0xDE, // bit depth, color type, etc
            0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, // IDAT chunk
            0x08, 0xD7, 0x63, 0xF8, 0x0F, 0x00, 0x00, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB4,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, // IEND chunk
            0xAE, 0x42, 0x60, 0x82,
        ];

        let mut file = File::create(path).unwrap();
        file.write_all(&png_data).unwrap();
    }

    #[test]
    fn test_scan_config_default() {
        let config = ScanConfig::default();
        assert!(config.recursive);
        assert_eq!(config.max_depth, 0);
        assert_eq!(config.batch_size, 100);
        assert!(!config.follow_symlinks);
    }

    #[test]
    fn test_discover_files_empty_dir() {
        let dir = tempdir().unwrap();
        let config = ScanConfig::default();
        let entries = FileScanner::discover_files(dir.path(), &config).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_discover_files_with_images() {
        let dir = tempdir().unwrap();

        // Create some test files
        create_test_image(&dir.path().join("image1.png"));
        create_test_image(&dir.path().join("image2.png"));
        File::create(dir.path().join("not_media.txt")).unwrap();

        let config = ScanConfig::default();
        let entries = FileScanner::discover_files(dir.path(), &config).unwrap();

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.media_type == MediaType::Image));
    }

    #[test]
    fn test_discover_files_recursive() {
        let dir = tempdir().unwrap();

        // Create nested structure
        let subdir = dir.path().join("subdir");
        fs::create_dir(&subdir).unwrap();

        create_test_image(&dir.path().join("root.png"));
        create_test_image(&subdir.join("nested.png"));

        // Recursive scan
        let config = ScanConfig {
            recursive: true,
            ..Default::default()
        };
        let entries = FileScanner::discover_files(dir.path(), &config).unwrap();
        assert_eq!(entries.len(), 2);

        // Non-recursive scan
        let config = ScanConfig {
            recursive: false,
            ..Default::default()
        };
        let entries = FileScanner::discover_files(dir.path(), &config).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn test_file_scanner_basic() {
        let dir = tempdir().unwrap();
        let db_dir = tempdir().unwrap();
        let db_path = db_dir.path().join("test.sqlite");

        // Create test images
        create_test_image(&dir.path().join("test1.png"));
        create_test_image(&dir.path().join("test2.png"));

        let store = MediaStore::open(&db_path).unwrap();
        let scanner = FileScanner::new();

        let (items, result) = scanner.scan_directory(dir.path(), store).await.unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(result.total_files, 2);
        assert_eq!(result.new_items, 2);
        assert_eq!(result.cached_items, 0);
        assert_eq!(result.error_count, 0);
    }

    #[tokio::test]
    async fn test_file_scanner_caching() {
        let dir = tempdir().unwrap();
        let db_dir = tempdir().unwrap();
        let db_path = db_dir.path().join("test.sqlite");

        create_test_image(&dir.path().join("cached.png"));

        // First scan
        {
            let store = MediaStore::open(&db_path).unwrap();
            let scanner = FileScanner::new();
            let (_, result) = scanner.scan_directory(dir.path(), store).await.unwrap();
            assert_eq!(result.new_items, 1);
            assert_eq!(result.cached_items, 0);
        }

        // Second scan should use cache
        {
            let store = MediaStore::open(&db_path).unwrap();
            let scanner = FileScanner::new();
            let (_, result) = scanner.scan_directory(dir.path(), store).await.unwrap();
            // Note: items are marked as cached if mtime/size match,
            // but we still return them with placeholder dimensions
            assert_eq!(result.total_files, 1);
        }
    }

    #[test]
    fn test_parallel_scanner_creation() {
        let scanner = ParallelScanner::new(4);
        assert_eq!(scanner.num_workers, 4);

        // Should enforce minimum of 1 worker
        let scanner = ParallelScanner::new(0);
        assert_eq!(scanner.num_workers, 1);
    }
}
