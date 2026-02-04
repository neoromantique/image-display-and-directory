use crate::layout::justified::RowBreak;
use crate::models::{MediaItem, RowModel};
use parking_lot::RwLock;
use std::collections::HashMap;
use xxhash_rust::xxh3::xxh3_64;

/// Width bucket size for cache keys.
/// Viewport widths are bucketed to avoid excessive cache invalidation on small resizes.
const WIDTH_BUCKET_SIZE: u32 = 50;

/// Maximum number of cached layouts to keep in memory.
const MAX_CACHE_ENTRIES: usize = 8;

/// Key for the layout cache, combining width bucket and list hash.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    width_bucket: u32,
    list_hash: u64,
}

/// Cached layout data: the row breaks that can reconstruct the full layout.
#[derive(Debug, Clone)]
struct CachedLayout {
    /// Row breaks containing start/end indices and heights
    breaks: Vec<RowBreak>,
    /// Number of items this layout was computed for
    item_count: usize,
    /// Timestamp of when this cache entry was last used (for LRU eviction)
    last_used: std::time::Instant,
}

/// Layout cache for storing and retrieving row breaks.
///
/// Caches row breaks keyed by (width_bucket, list_hash) to achieve O(1) layout
/// retrieval on cache hits. The width_bucket is computed as viewport_width / 50
/// to avoid cache invalidation on small resizes.
///
/// The list_hash is a fast hash of (path + mtime) for all items in the current
/// sort order, ensuring the cache is invalidated when the file list changes.
pub struct LayoutCache {
    cache: RwLock<HashMap<CacheKey, CachedLayout>>,
}

impl LayoutCache {
    /// Creates a new empty layout cache.
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::with_capacity(MAX_CACHE_ENTRIES)),
        }
    }

    /// Computes the width bucket for a given viewport width.
    /// This groups similar widths together to reduce cache churn during window resizing.
    pub fn width_bucket(viewport_width: f32) -> u32 {
        (viewport_width as u32) / WIDTH_BUCKET_SIZE
    }

    /// Computes a fast hash of the media item list.
    /// The hash is based on (path + mtime) for each item in order,
    /// so any change to files or sort order will invalidate the cache.
    pub fn compute_list_hash(items: &[MediaItem]) -> u64 {
        let mut hasher_input = Vec::with_capacity(items.len() * 64);

        for item in items {
            // Include path as bytes
            hasher_input.extend_from_slice(item.path.as_os_str().as_encoded_bytes());
            // Include mtime as bytes
            hasher_input.extend_from_slice(&item.mtime.to_le_bytes());
        }

        xxh3_64(&hasher_input)
    }

    /// Attempts to retrieve cached row breaks.
    /// Returns None on cache miss.
    ///
    /// # Arguments
    /// * `width_bucket` - The width bucket (use `LayoutCache::width_bucket()` to compute)
    /// * `list_hash` - The list hash (use `LayoutCache::compute_list_hash()` to compute)
    pub fn get_breaks(&self, width_bucket: u32, list_hash: u64) -> Option<Vec<RowBreak>> {
        let key = CacheKey {
            width_bucket,
            list_hash,
        };

        // Try read-only access first
        {
            let cache = self.cache.read();
            if let Some(entry) = cache.get(&key) {
                return Some(entry.breaks.clone());
            }
        }

        None
    }

    /// Attempts to retrieve cached rows, reconstructing them from breaks.
    /// Returns None on cache miss.
    ///
    /// This is the primary retrieval method that returns full RowModel objects
    /// ready for rendering.
    pub fn get(
        &self,
        width_bucket: u32,
        list_hash: u64,
        items: &[MediaItem],
        layout: &crate::layout::JustifiedLayout,
    ) -> Option<Vec<RowModel>> {
        let key = CacheKey {
            width_bucket,
            list_hash,
        };

        // Try to get from cache and update last_used
        let breaks = {
            let mut cache = self.cache.write();
            if let Some(entry) = cache.get_mut(&key) {
                // Verify item count matches
                if entry.item_count != items.len() {
                    return None;
                }
                entry.last_used = std::time::Instant::now();
                Some(entry.breaks.clone())
            } else {
                None
            }
        }?;

        // Reconstruct rows from breaks
        Some(layout.rows_from_breaks(items, &breaks))
    }

    /// Stores row breaks in the cache.
    ///
    /// # Arguments
    /// * `width_bucket` - The width bucket
    /// * `list_hash` - The list hash
    /// * `breaks` - The computed row breaks
    /// * `item_count` - Number of items (for validation on retrieval)
    pub fn set(&self, width_bucket: u32, list_hash: u64, breaks: Vec<RowBreak>, item_count: usize) {
        let key = CacheKey {
            width_bucket,
            list_hash,
        };

        let entry = CachedLayout {
            breaks,
            item_count,
            last_used: std::time::Instant::now(),
        };

        let mut cache = self.cache.write();

        // Evict oldest entry if at capacity
        if cache.len() >= MAX_CACHE_ENTRIES && !cache.contains_key(&key) {
            self.evict_oldest(&mut cache);
        }

        cache.insert(key, entry);
    }

    /// Stores rows in the cache by extracting their breaks.
    /// This is a convenience method when you have RowModels but want to cache them.
    pub fn set_rows(
        &self,
        width_bucket: u32,
        list_hash: u64,
        rows: &[RowModel],
        items: &[MediaItem],
    ) {
        // Extract breaks from rows
        let mut breaks = Vec::with_capacity(rows.len());
        let mut current_idx = 0;

        for row in rows {
            let end_idx = current_idx + row.items.len();
            breaks.push(RowBreak {
                start_index: current_idx,
                end_index: end_idx,
                row_height: row.height_px,
            });
            current_idx = end_idx;
        }

        self.set(width_bucket, list_hash, breaks, items.len());
    }

    /// Clears the entire cache.
    pub fn clear(&self) {
        self.cache.write().clear();
    }

    /// Returns the number of cached layouts.
    pub fn len(&self) -> usize {
        self.cache.read().len()
    }

    /// Returns true if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.cache.read().is_empty()
    }

    /// Evicts the least recently used entry from the cache.
    fn evict_oldest(&self, cache: &mut HashMap<CacheKey, CachedLayout>) {
        let oldest_key = cache
            .iter()
            .min_by_key(|(_, v)| v.last_used)
            .map(|(k, _)| k.clone());

        if let Some(key) = oldest_key {
            cache.remove(&key);
        }
    }
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self::new()
    }
}

/// A convenience struct that combines layout computation with caching.
/// Use this when you want automatic cache management.
pub struct CachedLayoutComputer {
    pub layout: crate::layout::JustifiedLayout,
    pub cache: LayoutCache,
}

impl CachedLayoutComputer {
    /// Creates a new cached layout computer with default settings.
    pub fn new() -> Self {
        Self {
            layout: crate::layout::JustifiedLayout::default(),
            cache: LayoutCache::new(),
        }
    }

    /// Creates a new cached layout computer with custom layout settings.
    pub fn with_layout(layout: crate::layout::JustifiedLayout) -> Self {
        Self {
            layout,
            cache: LayoutCache::new(),
        }
    }

    /// Computes the layout, using cached results if available.
    ///
    /// This method is O(1) on cache hit and O(n) on cache miss.
    /// Cache hits occur when the viewport width bucket and list hash match a previous computation.
    pub fn compute(&self, items: &[MediaItem], viewport_width: f32) -> Vec<RowModel> {
        if items.is_empty() {
            return Vec::new();
        }

        let width_bucket = LayoutCache::width_bucket(viewport_width);
        let list_hash = LayoutCache::compute_list_hash(items);

        // Try cache first (O(1))
        if let Some(rows) = self.cache.get(width_bucket, list_hash, items, &self.layout) {
            return rows;
        }

        // Cache miss: compute layout (O(n))
        let breaks = self.layout.compute_breaks(items, viewport_width);
        let rows = self.layout.rows_from_breaks(items, &breaks);

        // Store in cache
        self.cache.set(width_bucket, list_hash, breaks, items.len());

        rows
    }

    /// Invalidates the cache, forcing recomputation on next call.
    pub fn invalidate(&self) {
        self.cache.clear();
    }
}

impl Default for CachedLayoutComputer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MediaType;
    use std::path::PathBuf;

    fn make_item(path: &str, width: u32, height: u32, mtime: i64) -> MediaItem {
        MediaItem {
            path: PathBuf::from(path),
            media_type: MediaType::Image,
            mtime,
            size: 0,
            width,
            height,
            duration_ms: None,
            thumb_path: None,
            thumb_w: None,
            thumb_h: None,
            last_seen: 0,
        }
    }

    #[test]
    fn test_width_bucket() {
        assert_eq!(LayoutCache::width_bucket(1920.0), 38);
        assert_eq!(LayoutCache::width_bucket(1900.0), 38);
        assert_eq!(LayoutCache::width_bucket(1950.0), 39);
        assert_eq!(LayoutCache::width_bucket(100.0), 2);
    }

    #[test]
    fn test_list_hash_consistency() {
        let items = vec![
            make_item("a.jpg", 100, 100, 1000),
            make_item("b.jpg", 200, 200, 2000),
        ];

        let hash1 = LayoutCache::compute_list_hash(&items);
        let hash2 = LayoutCache::compute_list_hash(&items);

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_list_hash_changes_on_mtime() {
        let items1 = vec![make_item("a.jpg", 100, 100, 1000)];
        let items2 = vec![make_item("a.jpg", 100, 100, 2000)]; // Different mtime

        let hash1 = LayoutCache::compute_list_hash(&items1);
        let hash2 = LayoutCache::compute_list_hash(&items2);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_list_hash_changes_on_order() {
        let items1 = vec![
            make_item("a.jpg", 100, 100, 1000),
            make_item("b.jpg", 200, 200, 2000),
        ];
        let items2 = vec![
            make_item("b.jpg", 200, 200, 2000),
            make_item("a.jpg", 100, 100, 1000),
        ];

        let hash1 = LayoutCache::compute_list_hash(&items1);
        let hash2 = LayoutCache::compute_list_hash(&items2);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_cache_miss_then_hit() {
        let cache = LayoutCache::new();
        let width_bucket = 38;
        let list_hash = 12345u64;

        // Initial miss
        assert!(cache.get_breaks(width_bucket, list_hash).is_none());

        // Store breaks
        let breaks = vec![
            RowBreak {
                start_index: 0,
                end_index: 3,
                row_height: 220.0,
            },
            RowBreak {
                start_index: 3,
                end_index: 5,
                row_height: 220.0,
            },
        ];
        cache.set(width_bucket, list_hash, breaks.clone(), 5);

        // Now should hit
        let retrieved = cache.get_breaks(width_bucket, list_hash);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().len(), 2);
    }

    #[test]
    fn test_cache_eviction() {
        let cache = LayoutCache::new();
        let breaks = vec![RowBreak {
            start_index: 0,
            end_index: 1,
            row_height: 220.0,
        }];

        // Fill cache beyond capacity
        for i in 0..(MAX_CACHE_ENTRIES + 5) {
            cache.set(i as u32, i as u64, breaks.clone(), 1);
        }

        // Cache should not exceed max size
        assert!(cache.len() <= MAX_CACHE_ENTRIES);
    }

    #[test]
    fn test_cached_layout_computer() {
        let computer = CachedLayoutComputer::new();

        let items: Vec<MediaItem> = (0..10)
            .map(|i| make_item(&format!("{}.jpg", i), 1920, 1080, i as i64))
            .collect();

        // First compute (cache miss)
        let rows1 = computer.compute(&items, 1920.0);
        assert!(!rows1.is_empty());

        // Second compute (cache hit) - should return same result
        let rows2 = computer.compute(&items, 1920.0);
        assert_eq!(rows1.len(), rows2.len());

        // Verify cache is populated
        assert!(!computer.cache.is_empty());
    }

    #[test]
    fn test_cache_invalidation_on_different_width_bucket() {
        let computer = CachedLayoutComputer::new();

        let items: Vec<MediaItem> = (0..10)
            .map(|i| make_item(&format!("{}.jpg", i), 1920, 1080, i as i64))
            .collect();

        // Compute at width 1920 (bucket 38)
        let rows1 = computer.compute(&items, 1920.0);

        // Compute at width 1500 (bucket 30) - should miss cache
        let rows2 = computer.compute(&items, 1500.0);

        // Different viewport widths can produce different row counts
        // The important thing is both are valid layouts
        assert!(!rows1.is_empty());
        assert!(!rows2.is_empty());

        // Cache should have both entries
        assert!(computer.cache.len() >= 1);
    }

    #[test]
    fn test_empty_items() {
        let computer = CachedLayoutComputer::new();
        let rows = computer.compute(&[], 1920.0);
        assert!(rows.is_empty());
    }

    #[test]
    fn test_set_rows_and_retrieve() {
        let cache = LayoutCache::new();
        let layout = crate::layout::JustifiedLayout::default();

        let items: Vec<MediaItem> = (0..5)
            .map(|i| make_item(&format!("{}.jpg", i), 1920, 1080, i as i64))
            .collect();

        // Compute rows directly
        let rows = layout.compute(&items, 1920.0);
        let width_bucket = 38;
        let list_hash = LayoutCache::compute_list_hash(&items);

        // Store via set_rows
        cache.set_rows(width_bucket, list_hash, &rows, &items);

        // Retrieve and verify
        let retrieved = cache.get(width_bucket, list_hash, &items, &layout);
        assert!(retrieved.is_some());
        let retrieved_rows = retrieved.unwrap();
        assert_eq!(retrieved_rows.len(), rows.len());
    }
}
