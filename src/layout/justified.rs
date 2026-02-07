use crate::models::{MediaItem, RowItem, RowModel};

/// Configuration for the dense flow layout algorithm.
///
/// Items are placed left-to-right with fixed tile height and zero spacing.
/// Width follows source aspect ratio.
#[derive(Debug, Clone)]
pub struct JustifiedLayout {
    /// Target row height in pixels (default: 220)
    pub target_height: f32,
    /// Minimum allowed row height in pixels (default: 1)
    pub min_height: f32,
    /// Maximum allowed row height in pixels (default: 10000)
    pub max_height: f32,
    /// Gap between items in a row in pixels (default: 0)
    pub gap: f32,
}

impl Default for JustifiedLayout {
    fn default() -> Self {
        Self {
            target_height: 220.0,
            min_height: 1.0,
            max_height: 10_000.0,
            gap: 0.0,
        }
    }
}

impl JustifiedLayout {
    fn clamp_tile_height(&self, h: f32) -> f32 {
        h.clamp(self.min_height, self.max_height).max(1.0)
    }

    fn tile_dimensions(&self, item: &MediaItem) -> (f32, f32) {
        let mut ar = item.aspect_ratio().max(0.01);
        if item.is_video() && !(0.2..=5.0).contains(&ar) {
            // Some containers report junk dimensions; keep video tiles visible/stable in-grid.
            ar = 16.0 / 9.0;
        }
        let height = self.clamp_tile_height(self.target_height.max(1.0));
        let width = (height * ar).max(1.0);
        (width, height)
    }

    fn row_wrap_limit(&self, viewport_width: f32) -> f32 {
        viewport_width.max(1.0)
    }

    fn tile_offset_top(&self) -> f32 {
        0.0
    }

    /// Creates a new JustifiedLayout with custom parameters.
    #[cfg(test)]
    pub fn new(target_height: f32, min_height: f32, max_height: f32, gap: f32) -> Self {
        Self {
            target_height,
            min_height,
            max_height,
            gap,
        }
    }

    /// Computes a dense fixed-height flow layout for a list of media items.
    ///
    /// # Algorithm
    /// 1. Compute per-item tile dimensions using a shared tile height.
    /// 2. Stream items left-to-right with wrap at full viewport width.
    /// 3. Keep inter-item spacing at zero for a continuous packed chunk.
    ///
    /// # Arguments
    /// * `items` - Slice of MediaItems to layout
    /// * `viewport_width` - The available width in pixels
    ///
    /// # Returns
    /// A vector of RowModels describing each row's height and item dimensions.
    pub fn compute(&self, items: &[MediaItem], viewport_width: f32) -> Vec<RowModel> {
        if items.is_empty() || viewport_width <= 0.0 {
            return Vec::new();
        }

        let mut rows = Vec::new();
        let mut row_index = 0u32;
        let mut pending_items: Vec<RowItem> = Vec::new();
        let mut row_width = 0.0f32;
        let mut row_height = 1.0f32;
        let row_wrap_limit = self.row_wrap_limit(viewport_width);

        for item in items {
            let (item_w, item_h) = self.tile_dimensions(item);
            if !pending_items.is_empty() {
                let required_width = row_width + self.gap + item_w;
                if required_width > row_wrap_limit {
                    rows.push(RowModel::new(
                        row_index,
                        row_height,
                        std::mem::take(&mut pending_items),
                    ));
                    row_index += 1;
                    row_width = 0.0;
                    row_height = 1.0;
                }
            }

            let offset_top = self.tile_offset_top();
            if !pending_items.is_empty() {
                row_width += self.gap;
            }
            pending_items.push(RowItem {
                media_path: item.path.clone(),
                display_w: item_w,
                display_h: item_h,
                offset_top,
                is_folder: item.is_folder(),
            });
            row_width += item_w;
            row_height = row_height.max(item_h + offset_top);
        }

        if !pending_items.is_empty() {
            rows.push(RowModel::new(row_index, row_height, pending_items));
        }

        rows
    }

    /// Computes row breaks (indices) for caching purposes.
    /// Returns a vector of (start_index, end_index, row_height) tuples.
    ///
    /// This is useful for the layout cache to store minimal data.
    #[cfg(test)]
    pub fn compute_breaks(&self, items: &[MediaItem], viewport_width: f32) -> Vec<RowBreak> {
        let rows = self.compute(items, viewport_width);
        let mut start = 0usize;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let end = start + row.items.len();
            out.push(RowBreak {
                start_index: start,
                end_index: end,
                row_height: row.height_px,
            });
            start = end;
        }
        out
    }

    /// Reconstructs rows from cached breaks without re-running the layout algorithm.
    /// This is O(n) in the number of items but avoids the layout computation.
    #[cfg(test)]
    pub fn rows_from_breaks(&self, items: &[MediaItem], breaks: &[RowBreak]) -> Vec<RowModel> {
        breaks
            .iter()
            .enumerate()
            .map(|(row_idx, brk)| {
                let mut row_height = 1.0f32;
                let row_items: Vec<RowItem> = items[brk.start_index..brk.end_index]
                    .iter()
                    .map(|item| {
                        let (item_w, item_h) = self.tile_dimensions(item);
                        let offset_top = self.tile_offset_top();
                        row_height = row_height.max(item_h + offset_top);
                        RowItem {
                            media_path: item.path.clone(),
                            display_w: item_w,
                            display_h: item_h,
                            offset_top,
                            is_folder: item.is_folder(),
                        }
                    })
                    .collect();

                RowModel::new(row_idx as u32, row_height.max(brk.row_height), row_items)
            })
            .collect()
    }

    /// Calculates the total height of all rows.
    /// Useful for scroll calculations.
    #[cfg(test)]
    pub fn total_height(&self, rows: &[RowModel], row_gap: f32) -> f32 {
        if rows.is_empty() {
            return 0.0;
        }

        let heights_sum: f32 = rows.iter().map(|r| r.height_px).sum();
        let gaps_sum = (rows.len().saturating_sub(1)) as f32 * row_gap;
        heights_sum + gaps_sum
    }
}

/// Represents a row break for caching purposes.
/// Contains only the indices and height, not the actual items.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowBreak {
    /// Start index in the items array (inclusive)
    pub start_index: usize,
    /// End index in the items array (exclusive)
    pub end_index: usize,
    /// The computed height for this row
    pub row_height: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MediaType;
    use std::path::PathBuf;

    fn make_item(path: &str, width: u32, height: u32) -> MediaItem {
        MediaItem {
            path: PathBuf::from(path),
            media_type: MediaType::Image,
            mtime: 0,
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
    fn test_empty_items() {
        let layout = JustifiedLayout::default();
        let rows = layout.compute(&[], 1920.0);
        assert!(rows.is_empty());
    }

    #[test]
    fn test_single_item() {
        let layout = JustifiedLayout::default();
        let items = vec![make_item("a.jpg", 1920, 1080)];
        let rows = layout.compute(&items, 1920.0);

        assert_eq!(rows.len(), 1);
        let (_, expected_h) = layout.tile_dimensions(&items[0]);
        assert!((rows[0].height_px - expected_h).abs() < 0.01);
    }

    #[test]
    fn test_multiple_rows() {
        let layout = JustifiedLayout::default();
        let items: Vec<MediaItem> = (0..12)
            .map(|i| make_item(&format!("{}.jpg", i), 1920, 1080))
            .collect();

        let rows = layout.compute(&items, 1920.0);

        assert!(rows.len() > 1, "Expected > 1 rows, got {}", rows.len());
        let total_items: usize = rows.iter().map(|r| r.items.len()).sum();
        assert_eq!(total_items, items.len());
    }

    #[test]
    fn test_exact_row_fill() {
        let layout = JustifiedLayout::default();
        let items: Vec<MediaItem> = (0..10)
            .map(|i| make_item(&format!("{}.jpg", i), 1920, 1080))
            .collect();

        let rows = layout.compute(&items, 1920.0);

        assert!(rows.len() > 1);
        let total_items: usize = rows.iter().map(|r| r.items.len()).sum();
        assert_eq!(total_items, items.len());
    }

    #[test]
    fn test_panorama_keeps_uniform_height() {
        let layout = JustifiedLayout::default();
        let viewport = 3000.0;
        let items = vec![make_item("pano.jpg", 12000, 1000)];
        let rows = layout.compute(&items, viewport);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(
            (row.height_px - layout.target_height).abs() < 0.01,
            "expected panorama tile height to stay at target height"
        );
    }

    #[test]
    fn test_row_breaks_match_compute() {
        let layout = JustifiedLayout::default();
        let items: Vec<MediaItem> = (0..15)
            .map(|i| make_item(&format!("{}.jpg", i), 1920, 1080))
            .collect();

        let viewport_width = 1920.0;
        let rows = layout.compute(&items, viewport_width);
        let breaks = layout.compute_breaks(&items, viewport_width);

        // Same number of rows
        assert_eq!(rows.len(), breaks.len());

        // Heights should match
        for (row, brk) in rows.iter().zip(breaks.iter()) {
            assert!((row.height_px - brk.row_height).abs() < 0.01);
        }
    }

    #[test]
    fn test_rows_from_breaks() {
        let layout = JustifiedLayout::default();
        let items: Vec<MediaItem> = (0..15)
            .map(|i| make_item(&format!("{}.jpg", i), 1920, 1080))
            .collect();

        let viewport_width = 1920.0;
        let breaks = layout.compute_breaks(&items, viewport_width);
        let rows = layout.rows_from_breaks(&items, &breaks);
        let direct_rows = layout.compute(&items, viewport_width);

        // Same structure
        assert_eq!(rows.len(), direct_rows.len());
        for (r1, r2) in rows.iter().zip(direct_rows.iter()) {
            assert_eq!(r1.items.len(), r2.items.len());
            assert!((r1.height_px - r2.height_px).abs() < 0.01);
        }
    }

    #[test]
    fn test_mixed_aspect_ratios() {
        let layout = JustifiedLayout::default();
        let items = vec![
            make_item("wide.jpg", 1920, 1080),   // 16:9
            make_item("square.jpg", 1000, 1000), // 1:1
            make_item("tall.jpg", 1080, 1920),   // 9:16
            make_item("wide2.jpg", 2560, 1080),  // 21:9
        ];

        let rows = layout.compute(&items, 1920.0);

        // Verify all items are accounted for
        let total_items: usize = rows.iter().map(|r| r.items.len()).sum();
        assert_eq!(total_items, items.len());

        // All tiles should have a uniform height.
        for row in &rows {
            for item in &row.items {
                assert!((item.display_h - layout.target_height).abs() < 0.01);
            }
        }
    }

    #[test]
    fn test_height_clamping() {
        let layout = JustifiedLayout {
            target_height: 220.0,
            min_height: 140.0,
            max_height: 320.0,
            gap: 2.0,
        };

        // Create many small aspect ratio items (tall images)
        // This should try to create a very short row, testing min clamp
        let items: Vec<MediaItem> = (0..20)
            .map(|i| make_item(&format!("{}.jpg", i), 100, 400)) // Very tall
            .collect();

        let rows = layout.compute(&items, 1920.0);

        for row in &rows {
            assert!(
                row.height_px >= layout.min_height,
                "Row height {} below min {}",
                row.height_px,
                layout.min_height
            );
            for item in &row.items {
                assert!(
                    item.display_h >= layout.min_height && item.display_h <= layout.max_height,
                    "Item height {} outside [{}, {}]",
                    item.display_h,
                    layout.min_height,
                    layout.max_height
                );
                assert!(item.offset_top >= 0.0);
            }
        }
    }
}
