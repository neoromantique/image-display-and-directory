use crate::models::{MediaItem, RowItem, RowModel};

/// Configuration for the justified row layout algorithm.
///
/// This algorithm creates rows of images that fill the full viewport width,
/// maintaining consistent visual density while respecting aspect ratios.
#[derive(Debug, Clone)]
pub struct JustifiedLayout {
    /// Target row height in pixels (default: 220)
    pub target_height: f32,
    /// Minimum allowed row height in pixels (default: 140)
    pub min_height: f32,
    /// Maximum allowed row height in pixels (default: 320)
    pub max_height: f32,
    /// Gap between items in a row in pixels (default: 2)
    pub gap: f32,
}

impl Default for JustifiedLayout {
    fn default() -> Self {
        Self {
            target_height: 220.0,
            min_height: 140.0,
            max_height: 320.0,
            gap: 2.0,
        }
    }
}

impl JustifiedLayout {
    fn row_height_that_fits(
        &self,
        sum_ar: f32,
        num_gaps: f32,
        viewport_width: f32,
        last_row: bool,
    ) -> f32 {
        let available_width = (viewport_width - num_gaps * self.gap).max(1.0);
        let fit_height = (available_width / sum_ar).max(1.0);

        if last_row {
            return fit_height.min(self.target_height);
        }

        // Normal rows prefer the configured clamp range, but must never overflow.
        let mut row_height = fit_height.clamp(self.min_height, self.max_height);
        // Hard cap to ensure sum(widths) never exceeds available width.
        let max_fit_height = (available_width / sum_ar).max(1.0);
        if row_height > max_fit_height {
            row_height = max_fit_height;
        }
        row_height.max(1.0)
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

    /// Computes the justified row layout for a list of media items.
    ///
    /// # Algorithm
    /// 1. For each item in order, accumulate aspect ratios until the row would fill the viewport width.
    /// 2. Compute row_height = (viewport_width - gaps) / sum(aspect_ratios).
    /// 3. Clamp row_height to [min_height, max_height].
    /// 4. Compute each item's display width as aspect_ratio * row_height.
    /// 5. The last row uses target_height as an upper bound and may shrink to fit width.
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
        let mut row_index: u32 = 0;

        // Pending items for the current row
        let mut pending_items: Vec<&MediaItem> = Vec::new();
        let mut sum_ar: f32 = 0.0;

        for item in items {
            let ar = item.aspect_ratio();
            pending_items.push(item);
            sum_ar += ar;

            // Calculate required width if we use target height
            // Each item width = ar * target_height
            // Total width = sum_ar * target_height + gaps
            let num_gaps = (pending_items.len().saturating_sub(1)) as f32;
            let required_width = sum_ar * self.target_height + num_gaps * self.gap;

            // Check if this row is full (would exceed viewport width)
            if required_width >= viewport_width {
                // Finalize this row
                let row = self.finalize_row(
                    &pending_items,
                    sum_ar,
                    viewport_width,
                    row_index,
                    false, // not last row
                );
                rows.push(row);
                row_index += 1;

                // Reset for next row
                pending_items.clear();
                sum_ar = 0.0;
            }
        }

        // Handle the last row (incomplete row may shrink; never exceed viewport width)
        if !pending_items.is_empty() {
            let row = self.finalize_row(
                &pending_items,
                sum_ar,
                viewport_width,
                row_index,
                true, // last row
            );
            rows.push(row);
        }

        rows
    }

    /// Computes row breaks (indices) for caching purposes.
    /// Returns a vector of (start_index, end_index, row_height) tuples.
    ///
    /// This is useful for the layout cache to store minimal data.
    #[cfg(test)]
    pub fn compute_breaks(&self, items: &[MediaItem], viewport_width: f32) -> Vec<RowBreak> {
        if items.is_empty() || viewport_width <= 0.0 {
            return Vec::new();
        }

        let mut breaks = Vec::new();
        let mut row_start: usize = 0;
        let mut sum_ar: f32 = 0.0;
        let mut pending_count: usize = 0;

        for (idx, item) in items.iter().enumerate() {
            let ar = item.aspect_ratio();
            sum_ar += ar;
            pending_count += 1;

            let num_gaps = pending_count.saturating_sub(1) as f32;
            let required_width = sum_ar * self.target_height + num_gaps * self.gap;

            if required_width >= viewport_width {
                // Compute the actual row height for this complete row
                let row_height = self.row_height_that_fits(sum_ar, num_gaps, viewport_width, false);

                breaks.push(RowBreak {
                    start_index: row_start,
                    end_index: idx + 1, // exclusive
                    row_height,
                });

                row_start = idx + 1;
                sum_ar = 0.0;
                pending_count = 0;
            }
        }

        // Last row (incomplete, shrink if needed to avoid overflow)
        if pending_count > 0 {
            let num_gaps = pending_count.saturating_sub(1) as f32;
            let row_height = self.row_height_that_fits(sum_ar, num_gaps, viewport_width, true);
            breaks.push(RowBreak {
                start_index: row_start,
                end_index: items.len(),
                row_height,
            });
        }

        breaks
    }

    /// Reconstructs rows from cached breaks without re-running the layout algorithm.
    /// This is O(n) in the number of items but avoids the layout computation.
    #[cfg(test)]
    pub fn rows_from_breaks(&self, items: &[MediaItem], breaks: &[RowBreak]) -> Vec<RowModel> {
        breaks
            .iter()
            .enumerate()
            .map(|(row_idx, brk)| {
                let row_items: Vec<RowItem> = items[brk.start_index..brk.end_index]
                    .iter()
                    .map(|item| {
                        let ar = item.aspect_ratio();
                        RowItem {
                            media_path: item.path.clone(),
                            display_w: ar * brk.row_height,
                            display_h: brk.row_height,
                            is_folder: item.is_folder(),
                        }
                    })
                    .collect();

                RowModel::new(row_idx as u32, brk.row_height, row_items)
            })
            .collect()
    }

    /// Finalizes a row by computing the actual row height and item dimensions.
    fn finalize_row(
        &self,
        pending_items: &[&MediaItem],
        sum_ar: f32,
        viewport_width: f32,
        row_index: u32,
        is_last_row: bool,
    ) -> RowModel {
        let num_gaps = (pending_items.len().saturating_sub(1)) as f32;

        let row_height = self.row_height_that_fits(sum_ar, num_gaps, viewport_width, is_last_row);

        // Compute each item's display dimensions
        let row_items: Vec<RowItem> = pending_items
            .iter()
            .map(|item| {
                let ar = item.aspect_ratio();
                RowItem {
                    media_path: item.path.clone(),
                    display_w: ar * row_height,
                    display_h: row_height,
                    is_folder: item.is_folder(),
                }
            })
            .collect();

        RowModel::new(row_index, row_height, row_items)
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
        // Single item in last row uses target height
        assert!((rows[0].height_px - layout.target_height).abs() < 0.01);
    }

    #[test]
    fn test_multiple_rows() {
        let layout = JustifiedLayout::default();
        // Create items that will fill multiple rows with an incomplete last row
        // 16:9 aspect ratio items (ar = 1.78)
        // With viewport 1920 and target 220, about 4.9 items fit per row
        // So 5 items = 1 complete row, 7 items = 1 complete + 2 incomplete
        let items: Vec<MediaItem> = (0..7)
            .map(|i| make_item(&format!("{}.jpg", i), 1920, 1080))
            .collect();

        let rows = layout.compute(&items, 1920.0);

        // Should have multiple rows (1 complete + 1 incomplete)
        assert!(rows.len() > 1, "Expected > 1 rows, got {}", rows.len());

        // All rows except last should have height in valid range
        for (i, row) in rows[..rows.len() - 1].iter().enumerate() {
            assert!(
                row.height_px >= layout.min_height,
                "Row {} height {} < min {}",
                i,
                row.height_px,
                layout.min_height
            );
            assert!(
                row.height_px <= layout.max_height,
                "Row {} height {} > max {}",
                i,
                row.height_px,
                layout.max_height
            );
        }

        // Last row uses target height (since it's incomplete)
        let last_row = rows.last().unwrap();
        assert!(
            (last_row.height_px - layout.target_height).abs() < 0.01,
            "Last row height {} != target {}",
            last_row.height_px,
            layout.target_height
        );
    }

    #[test]
    fn test_exact_row_fill() {
        let layout = JustifiedLayout::default();
        // With 10 items at 16:9 and viewport 1920, we get exactly 2 complete rows
        // Both rows should be complete, so both should have computed height (not target)
        let items: Vec<MediaItem> = (0..10)
            .map(|i| make_item(&format!("{}.jpg", i), 1920, 1080))
            .collect();

        let rows = layout.compute(&items, 1920.0);

        // Should have exactly 2 rows
        assert_eq!(rows.len(), 2);

        // Both rows are complete, so both should have height within clamp range
        for row in &rows {
            assert!(row.height_px >= layout.min_height);
            assert!(row.height_px <= layout.max_height);
        }
    }

    #[test]
    fn test_incomplete_row_panoramas_do_not_overflow_viewport() {
        let layout = JustifiedLayout::default();
        let viewport = 1200.0;
        let items = vec![make_item("pano.jpg", 12000, 1000)];
        let rows = layout.compute(&items, viewport);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        let total_width: f32 = row.items.iter().map(|i| i.display_w).sum();
        assert!(
            total_width <= viewport + 0.5,
            "row width {} exceeded viewport {}",
            total_width,
            viewport
        );
        assert!(
            row.height_px < layout.target_height,
            "expected panorama last row to shrink below target height"
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

        for row in &rows[..rows.len().saturating_sub(1)] {
            assert!(
                row.height_px >= layout.min_height,
                "Row height {} below min {}",
                row.height_px,
                layout.min_height
            );
        }
    }
}
