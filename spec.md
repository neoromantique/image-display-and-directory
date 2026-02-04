# idxd Implementation Plan (Fast, Snappy Media Browser)

## Goals (Priority Ordered)
1) Fast and snappy runtime: smooth scrolling and instant-feeling navigation.
2) No gaps, full-width layout using justified rows (natural last row).
3) Reliable cache for metadata + thumbnails using SQLite + on-disk thumb files.
4) Solid image and WebM viewing experience.
5) First-run can be slower; subsequent runs must be near-instant.

## Target Hardware
- Low end: Intel N150 (limited CPU)
- High end: Ryzen 3700X + NVIDIA 1650
- Typical directories: hundreds to low thousands of files

## Key Decisions
- Layout: justified-rows masonry using ListView virtualization.
- Last row: natural height (not stretched to full width).
- Cache location:
  - SQLite metadata: XDG_CONFIG_HOME/idxd/cache.sqlite
  - Thumbnails as files: XDG_CACHE_HOME/idxd/thumbs/
- Runtime > first-run: use cached metadata and row breaks to avoid re-layout work.

## Non-Goals (for MVP)
- Full Pinterest-style column masonry (would reduce virtualization and increase hitching risk).
- Complex filters, tags, or metadata editing.
- Network sources or remote browsing.

## App Overview
- GTK4 UI with ListView + custom row widget.
- Async scanner + metadata extractor.
- Thumbnail generator with bounded queue and LRU in-memory cache.
- Viewer mode for full-size image and WebM via libmpv.

## Project Structure (Proposed)
src/
- main.rs
- app.rs
- models/
  - media_item.rs
  - media_store.rs
  - row_model.rs
- scanner/
  - file_scanner.rs
  - metadata.rs
- thumbnails/
  - generator.rs
  - cache.rs
  - queue.rs
- layout/
  - justified.rs
  - layout_cache.rs
- ui/
  - window.rs
  - list_view.rs
  - row_widget.rs
  - viewer.rs
  - keybindings.rs
- video/
  - player.rs

## Data Model
media_item
- path: String
- media_type: Image | Video
- mtime: i64
- size: i64
- width: u32
- height: u32
- duration_ms: Option<u32>
- thumb_path: Option<String>
- thumb_w, thumb_h: Option<u32>
- last_seen: i64

row_model
- row_index: u32
- height_px: f32
- items: Vec<RowItem>

row_item
- media_id (or path)
- display_w: f32
- display_h: f32

## SQLite Cache Schema
Table: media
- path TEXT PRIMARY KEY
- media_type INTEGER
- mtime INTEGER
- size INTEGER
- width INTEGER
- height INTEGER
- duration_ms INTEGER
- thumb_path TEXT
- thumb_w INTEGER
- thumb_h INTEGER
- last_seen INTEGER

Table: layout_meta
- width_bucket INTEGER
- sort_key TEXT
- item_count INTEGER
- list_hash TEXT
- updated_at INTEGER

Table: layout_rows
- width_bucket INTEGER
- sort_key TEXT
- row_index INTEGER
- row_height REAL
- start_index INTEGER
- end_index INTEGER

Notes:
- list_hash is a fast hash of (path + mtime) in current sort order.
- If list_hash matches, reuse row breaks; otherwise recompute.

SQLite settings:
- journal_mode = WAL
- synchronous = NORMAL
- temp_store = MEMORY
- cache_size tuned for fast reads

## File Scanning and Metadata
- Scan directory recursively (configurable) using async I/O.
- For each file:
  - Determine type by extension (jpg, png, webp, gif, bmp, tiff, webm, mp4).
  - Read mtime/size first; use cached metadata if unchanged.
  - For new/changed items: extract dimensions quickly.
  - Write metadata to SQLite in batches.
- The scanner should never block UI.

## Layout: Justified Rows (No Gaps)
- Keep target row height (e.g., 200-240px) with min/max clamps (140-320px).
- For each item in order, accumulate aspect ratios until row fills viewport width.
- Compute row height = viewport_width / sum(aspect_ratios).
- Clamp row height; adjust row widths proportionally.
- Last row uses target height (not stretched to full width).

Pseudo algorithm:
- sum_ar = 0; row_items = []
- for item in items:
  - ar = width / height
  - push item; sum_ar += ar
  - if sum_ar * target_height >= viewport_width:
    - row_height = viewport_width / sum_ar
    - clamp row_height
    - set each item display_w = ar * row_height
    - emit row
    - reset
- last row: use target_height and do not stretch

Row caching:
- Cache row breaks (start/end indices + row height) keyed by width_bucket and list_hash.
- On resize, recompute only when width_bucket changes.

## UI Rendering
- Use GTK4 ListView with a row model (ListStore of row_model).
- Each row widget is a horizontal container of child Picture widgets.
- Avoid per-item widget creation on every scroll by using a reusable factory.
- Each Picture uses a placeholder texture until thumbnail arrives.

## Thumbnail Pipeline
- Thumbnails stored as files in XDG_CACHE_HOME/idxd/thumbs/.
- Filename based on hash(path + mtime + size).
- Generate on-demand only for visible rows + small prefetch margin.
- Decode/rescale on a bounded worker pool (2-3 threads on low end).
- Update GTK textures in batches on the main thread.

Thumbnail sizes:
- Default thumb height ~ 256px (adjustable).
- Store exact generated size to avoid re-scaling in UI.

Memory cache:
- LRU cache of GdkTexture with strict size limit (e.g., 128-256 MB).
- Evict on memory pressure or when exceeding cap.

## Viewer (Image)
- On Enter: show viewer with fast preview (thumbnail-sized or downscaled decode).
- Load full-res in background and swap when ready.
- Zoom/pan on image with smooth scaling (limit max scale).
- Keep viewer responsive; never block UI thread.

## Viewer (WebM/Video)
- Use libmpv embedded in a GTK widget.
- Keep mpv instance alive across selections to avoid re-init overhead.
- Enable hardware decode by default; fallback gracefully.
- Basic controls: play/pause, seek, next/prev.

## Input/Keybindings (MVP)
- Grid navigation: arrows + hjkl.
- Enter: open viewer.
- Escape: close viewer to grid.
- Space: play/pause (video) or toggle UI.

## Performance Budget
- Time-to-grid: < 500ms on cached runs.
- Scroll: no frame drops during fast scrolling.
- Open viewer: < 100ms to first pixels (preview).
- Layout: O(n) on cache miss; O(1) on cache hit.

## Runtime Concurrency
- Tokio runtime for scanning and I/O.
- Dedicated worker pool for thumbnail decode.
- UI updates batched via timeout (e.g., 16-33ms) to avoid per-item redraw overhead.

## Error Handling
- Broken images: mark as error state, show fallback icon.
- Missing thumbnail file: regenerate.
- SQLite corruption: rename + rebuild, do not crash.

## Milestones
1) Skeleton app: GTK window + list view + placeholder rows.
2) Scanner + metadata cache in SQLite.
3) Justified layout + row cache.
4) Thumbnail pipeline + disk cache.
5) Viewer (image).
6) Viewer (WebM) via mpv.
7) Keybindings and polish.

## Testing Plan
- Manual tests on both low and high end hardware:
  - 100, 500, 2000 files
  - Mixed aspect ratios
  - Rapid scroll with constant thumbnail load
  - Resize window repeatedly
  - Open/close viewer rapidly

## Notes
- If a future requirement demands true column masonry, consider a custom layout widget,
  but keep justified rows as the performance baseline.

## Terminal Aesthetic Rules (IMPORTANT)

These rules define the visual style. **Do not deviate from these.**
Apply via GTK4 CSS (GtkCssProvider) and widget style classes.

### No Material Design (GTK4)

- **NO solid filled buttons** - use outlined/bordered buttons with transparent backgrounds
- **NO rounded corners** - set `border-radius: 0` globally
- **NO drop shadows** - `box-shadow: none`
- **NO transform/scale on hover** - only change border color
- **NO gradients** - `background-image: none` and flat colors only
- **NO emoji** - use plain text or ASCII (`>`, `[ ]`, `OK`)

### Button Styling (GTK CSS)

```css
/* Primary button - outlined, not filled */
button.btn-primary {
    background-color: transparent;
    border: 1px solid @accent_primary;
    color: @accent_primary;
}

button.btn-primary:hover {
    background-color: alpha(@accent_primary, 0.10);
    border-color: @accent_primary;
}
```

### Card/Panel Styling (GTK CSS)

```css
/* Apply to GtkBox/GtkFrame with style class "card" or "panel" */
.card, .panel {
    border: 1px solid @border_color;
    border-radius: 0;
    background-color: @bg_secondary;
    box-shadow: none;
}
```

### Interactive Elements

- Hover state: change `border-color` only, no transforms
- Active state: use dashed borders for dragging or selection
- Focus: use `outline` or `border-color`, not shadow

### Typography

- Section titles: uppercase with letter spacing (Pango attributes if needed)
- Labels: small, uppercase, muted color
- List-like labels should use `> ` prefix instead of emoji

### Layout Rules

1. **Use available space** - grid/rows fill width (3-4 items per row typical)
2. **Compact cards** - info-dense, minimal padding
3. **Action buttons at bottom** - viewer controls anchored consistently
4. **Visible state** - always show actions; change border/label text for state (e.g., `OK` vs `[ ]`)
