//! Thumbnail worker queue for async generation.
//!
//! - Bounded worker pool (2-3 threads) for thumbnail generation
//! - Generate on-demand for visible rows + prefetch margin
//! - Update GTK textures in batches on main thread
//! - Uses flume for communication between workers and main thread

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use flume::{Receiver, Sender};
use gdk4::Texture;
use glib::ControlFlow;
use parking_lot::{Mutex, RwLock};
use tracing::{debug, error, trace, warn};

use super::cache::{CacheKey, ThumbnailCache};

/// Default number of worker threads.
const DEFAULT_WORKERS: usize = 2;

/// Maximum number of worker threads.
const MAX_WORKERS: usize = 4;

/// Batch update interval in milliseconds for GTK texture updates.
const BATCH_UPDATE_MS: u32 = 16; // ~60fps

/// Maximum number of pending requests in the queue.
const MAX_QUEUE_SIZE: usize = 256;

/// Number of rows to prefetch ahead of the visible area.
const PREFETCH_MARGIN: usize = 3;

/// A request to generate a thumbnail.
#[derive(Debug, Clone)]
pub struct ThumbnailRequest {
    /// Path to the source image.
    pub path: PathBuf,
    /// File modification time.
    pub mtime: i64,
    /// File size in bytes.
    pub size: i64,
    /// Priority (lower = higher priority, used for visible items).
    pub priority: u32,
    /// Row index for this item (used for batching).
    pub row_index: u32,
}

impl ThumbnailRequest {
    pub fn new(path: PathBuf, mtime: i64, size: i64) -> Self {
        Self {
            path,
            mtime,
            size,
            priority: 100,
            row_index: 0,
        }
    }

    pub fn with_priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_row(mut self, row_index: u32) -> Self {
        self.row_index = row_index;
        self
    }

    /// Get the cache key for this request.
    pub fn cache_key(&self) -> CacheKey {
        CacheKey::new(&self.path, self.mtime, self.size)
    }
}

/// Result of thumbnail generation sent back to the main thread.
#[derive(Debug, Clone)]
pub struct ThumbnailResult {
    /// Original request.
    pub path: PathBuf,
    /// File modification time (for verification).
    pub mtime: i64,
    /// File size (for verification).
    pub size: i64,
    /// The loaded texture, if successful.
    pub texture: Option<Texture>,
    /// Thumbnail dimensions.
    pub width: u32,
    pub height: u32,
    /// Error message if generation failed.
    pub error: Option<String>,
}

/// Callback type for handling completed thumbnails.
pub type ThumbnailCallback = Box<dyn Fn(ThumbnailResult) + Send + Sync>;

/// Worker queue for thumbnail generation.
pub struct ThumbnailQueue {
    /// Sender for new requests.
    request_tx: Sender<ThumbnailRequest>,
    /// Receiver for completed results (main thread reads this).
    result_rx: Receiver<ThumbnailResult>,
    /// Worker thread handles.
    workers: Vec<JoinHandle<()>>,
    /// Flag to signal workers to stop.
    shutdown: Arc<AtomicBool>,
    /// Number of active workers.
    active_workers: Arc<AtomicUsize>,
    /// Set of paths currently being processed (to avoid duplicates).
    pending: Arc<RwLock<HashSet<PathBuf>>>,
    /// The thumbnail cache shared with workers.
    cache: ThumbnailCache,
    /// Currently visible row range for prioritization.
    visible_range: Arc<RwLock<(u32, u32)>>,
    /// Callbacks for thumbnail completion.
    callbacks: Arc<Mutex<Vec<ThumbnailCallback>>>,
    /// GTK source ID for batch updates (if active).
    batch_source_id: Arc<Mutex<Option<glib::SourceId>>>,
}

impl ThumbnailQueue {
    /// Create a new thumbnail queue with the specified number of workers.
    pub fn new(workers: usize) -> Self {
        let cache = ThumbnailCache::new_default(192).expect("Failed to create thumbnail cache");
        Self::with_cache(workers, cache)
    }

    /// Create a new thumbnail queue with a custom cache.
    pub fn with_cache(workers: usize, cache: ThumbnailCache) -> Self {
        let num_workers = workers.clamp(1, MAX_WORKERS);

        let (request_tx, request_rx) = flume::bounded(MAX_QUEUE_SIZE);
        let (result_tx, result_rx) = flume::unbounded();

        let shutdown = Arc::new(AtomicBool::new(false));
        let active_workers = Arc::new(AtomicUsize::new(0));
        let pending = Arc::new(RwLock::new(HashSet::new()));

        let mut worker_handles = Vec::with_capacity(num_workers);

        // Spawn worker threads
        for worker_id in 0..num_workers {
            let rx = request_rx.clone();
            let tx = result_tx.clone();
            let shutdown = Arc::clone(&shutdown);
            let active = Arc::clone(&active_workers);
            let pending = Arc::clone(&pending);
            let cache = cache.clone();

            let handle = thread::Builder::new()
                .name(format!("thumb-worker-{}", worker_id))
                .spawn(move || {
                    worker_loop(worker_id, rx, tx, shutdown, active, pending, cache);
                })
                .expect("Failed to spawn thumbnail worker");

            worker_handles.push(handle);
        }

        debug!(num_workers, "Started thumbnail worker queue");

        Self {
            request_tx,
            result_rx,
            workers: worker_handles,
            shutdown,
            active_workers,
            pending,
            cache,
            visible_range: Arc::new(RwLock::new((0, 0))),
            callbacks: Arc::new(Mutex::new(Vec::new())),
            batch_source_id: Arc::new(Mutex::new(None)),
        }
    }

    /// Submit a request for thumbnail generation.
    ///
    /// Returns false if the queue is full or the item is already pending.
    pub fn request(&self, req: ThumbnailRequest) -> bool {
        // Check if already pending
        {
            let pending = self.pending.read();
            if pending.contains(&req.path) {
                trace!(?req.path, "Request already pending");
                return false;
            }
        }

        // Check if already cached
        if self.cache.exists(&req.path, req.mtime, req.size) {
            trace!(?req.path, "Thumbnail already cached");
            return false;
        }

        // Add to pending set
        self.pending.write().insert(req.path.clone());

        // Send to workers
        match self.request_tx.try_send(req) {
            Ok(_) => true,
            Err(flume::TrySendError::Full(req)) => {
                warn!("Thumbnail queue full, dropping request");
                self.pending.write().remove(&req.path);
                false
            }
            Err(flume::TrySendError::Disconnected(_)) => {
                error!("Thumbnail queue disconnected");
                false
            }
        }
    }

    /// Request thumbnails for multiple items.
    pub fn request_batch(&self, requests: Vec<ThumbnailRequest>) -> usize {
        let mut submitted = 0;
        for req in requests {
            if self.request(req) {
                submitted += 1;
            }
        }
        submitted
    }

    /// Request thumbnails for visible rows plus prefetch margin.
    pub fn request_visible_rows(
        &self,
        rows: &[(u32, Vec<(PathBuf, i64, i64)>)], // (row_index, [(path, mtime, size)])
        first_visible: u32,
        last_visible: u32,
    ) -> usize {
        // Update visible range
        *self.visible_range.write() = (first_visible, last_visible);

        // Calculate prefetch range
        let prefetch_start = first_visible.saturating_sub(PREFETCH_MARGIN as u32);
        let prefetch_end = last_visible.saturating_add(PREFETCH_MARGIN as u32);

        let mut requests = Vec::new();

        for (row_index, items) in rows {
            if *row_index < prefetch_start || *row_index > prefetch_end {
                continue;
            }

            // Calculate priority based on distance from visible range
            let priority = if *row_index >= first_visible && *row_index <= last_visible {
                0 // Visible items get highest priority
            } else {
                // Prefetch items get lower priority based on distance
                let distance = if *row_index < first_visible {
                    first_visible - *row_index
                } else {
                    *row_index - last_visible
                };
                distance + 1
            };

            for (path, mtime, size) in items {
                let req = ThumbnailRequest::new(path.clone(), *mtime, *size)
                    .with_priority(priority)
                    .with_row(*row_index);
                requests.push(req);
            }
        }

        // Sort by priority (lower = higher priority)
        requests.sort_by_key(|r| r.priority);

        self.request_batch(requests)
    }

    /// Poll for completed thumbnails (non-blocking).
    pub fn poll_results(&self) -> Vec<ThumbnailResult> {
        let mut results = Vec::new();
        while let Ok(result) = self.result_rx.try_recv() {
            results.push(result);
        }
        results
    }

    /// Register a callback to be called when thumbnails are ready.
    pub fn on_thumbnail_ready<F>(&self, callback: F)
    where
        F: Fn(ThumbnailResult) + Send + Sync + 'static,
    {
        self.callbacks.lock().push(Box::new(callback));
    }

    /// Start batch processing of results on the GTK main thread.
    ///
    /// This sets up a timeout that regularly checks for completed thumbnails
    /// and invokes callbacks.
    pub fn start_batch_processing(&self) {
        let result_rx = self.result_rx.clone();
        let callbacks = Arc::clone(&self.callbacks);
        let batch_source_id = Arc::clone(&self.batch_source_id);

        // Set up periodic polling on the main thread
        let source_id =
            glib::timeout_add_local(Duration::from_millis(BATCH_UPDATE_MS as u64), move || {
                // Process all available results
                while let Ok(result) = result_rx.try_recv() {
                    let cbs = callbacks.lock();
                    for cb in cbs.iter() {
                        cb(result.clone());
                    }
                }
                ControlFlow::Continue
            });

        *batch_source_id.lock() = Some(source_id);
        debug!("Started thumbnail batch processing");
    }

    /// Stop batch processing.
    pub fn stop_batch_processing(&self) {
        if let Some(source_id) = self.batch_source_id.lock().take() {
            source_id.remove();
            debug!("Stopped thumbnail batch processing");
        }
    }

    /// Get the thumbnail cache.
    pub fn cache(&self) -> &ThumbnailCache {
        &self.cache
    }

    /// Get the number of pending requests.
    pub fn pending_count(&self) -> usize {
        self.pending.read().len()
    }

    /// Get the number of active workers currently processing.
    pub fn active_worker_count(&self) -> usize {
        self.active_workers.load(Ordering::Relaxed)
    }

    /// Check if there is work in progress.
    pub fn is_busy(&self) -> bool {
        !self.pending.read().is_empty() || self.active_worker_count() > 0
    }

    /// Cancel all pending requests.
    ///
    /// Note: This only clears the pending set and prevents new requests from being submitted
    /// for the same paths. Requests already sent to workers will still be processed.
    pub fn cancel_all(&self) {
        // Clear pending set to allow re-submission
        self.pending.write().clear();

        debug!("Cancelled all pending thumbnail requests");
    }

    /// Shutdown the worker queue.
    pub fn shutdown(&mut self) {
        debug!("Shutting down thumbnail queue");

        // Signal shutdown
        self.shutdown.store(true, Ordering::SeqCst);

        // Stop batch processing
        self.stop_batch_processing();

        // Wait for workers to finish (with timeout)
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }

        debug!("Thumbnail queue shutdown complete");
    }
}

impl Drop for ThumbnailQueue {
    fn drop(&mut self) {
        if !self.shutdown.load(Ordering::Relaxed) {
            self.shutdown();
        }
    }
}

/// Worker thread loop.
fn worker_loop(
    worker_id: usize,
    rx: Receiver<ThumbnailRequest>,
    tx: Sender<ThumbnailResult>,
    shutdown: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    pending: Arc<RwLock<HashSet<PathBuf>>>,
    cache: ThumbnailCache,
) {
    debug!(worker_id, "Thumbnail worker started");

    loop {
        // Check for shutdown
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Wait for a request with timeout
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(req) => {
                active.fetch_add(1, Ordering::Relaxed);

                let result = process_request(&req, &cache);

                // Remove from pending
                pending.write().remove(&req.path);

                // Send result
                if let Err(e) = tx.send(result) {
                    warn!(worker_id, error = ?e, "Failed to send thumbnail result");
                }

                active.fetch_sub(1, Ordering::Relaxed);
            }
            Err(flume::RecvTimeoutError::Timeout) => {
                // No work, continue loop
                continue;
            }
            Err(flume::RecvTimeoutError::Disconnected) => {
                // Channel closed, exit
                break;
            }
        }
    }

    debug!(worker_id, "Thumbnail worker stopped");
}

/// Process a single thumbnail request.
fn process_request(req: &ThumbnailRequest, cache: &ThumbnailCache) -> ThumbnailResult {
    trace!(?req.path, "Processing thumbnail request");

    match cache.get_or_generate(&req.path, req.mtime, req.size) {
        Ok(cached) => ThumbnailResult {
            path: req.path.clone(),
            mtime: req.mtime,
            size: req.size,
            texture: Some(cached.texture),
            width: cached.width,
            height: cached.height,
            error: None,
        },
        Err(e) => {
            warn!(?req.path, error = ?e, "Failed to generate thumbnail");
            ThumbnailResult {
                path: req.path.clone(),
                mtime: req.mtime,
                size: req.size,
                texture: None,
                width: 0,
                height: 0,
                error: Some(e.to_string()),
            }
        }
    }
}

/// Builder for ThumbnailQueue with configuration options.
pub struct ThumbnailQueueBuilder {
    workers: usize,
    max_memory_mb: usize,
    cache_dir: Option<PathBuf>,
}

impl ThumbnailQueueBuilder {
    pub fn new() -> Self {
        Self {
            workers: DEFAULT_WORKERS,
            max_memory_mb: 192,
            cache_dir: None,
        }
    }

    pub fn workers(mut self, count: usize) -> Self {
        self.workers = count;
        self
    }

    pub fn max_memory_mb(mut self, mb: usize) -> Self {
        self.max_memory_mb = mb;
        self
    }

    pub fn cache_dir(mut self, dir: PathBuf) -> Self {
        self.cache_dir = Some(dir);
        self
    }

    pub fn build(self) -> anyhow::Result<ThumbnailQueue> {
        let cache = if let Some(dir) = self.cache_dir {
            ThumbnailCache::new(dir, self.max_memory_mb)
        } else {
            ThumbnailCache::new_default(self.max_memory_mb)?
        };

        Ok(ThumbnailQueue::with_cache(self.workers, cache))
    }
}

impl Default for ThumbnailQueueBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_priority() {
        let req = ThumbnailRequest::new("/test/image.jpg".into(), 123, 456)
            .with_priority(10)
            .with_row(5);

        assert_eq!(req.priority, 10);
        assert_eq!(req.row_index, 5);
    }

    #[test]
    fn test_cache_key_from_request() {
        let req = ThumbnailRequest::new("/test/image.jpg".into(), 123, 456);
        let key = req.cache_key();
        assert!(!key.disk_filename().is_empty());
    }
}
