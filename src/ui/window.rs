// Main window for idxd media browser
// GTK4 ApplicationWindow with ListView, Viewer, and terminal aesthetic CSS

use gdk4::Display;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, CssProvider, Label, Orientation, Stack,
    StackTransitionType, STYLE_PROVIDER_PRIORITY_APPLICATION,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::Duration;

use super::keybindings::{Keybindings, ViewMode};
use super::list_view::MediaListView;
use super::viewer::{MediaViewer, PrefetchItem, PrefetchKind};
use crate::layout::JustifiedLayout;
use crate::models::{MediaItem, MediaType, RowModel};

const SMALL_IMAGE_MAX_PIXELS: u64 = 2_000_000;
const PREFETCH_RADIUS: usize = 12;

struct DirectoryScanResult {
    items: Vec<MediaItem>,
    rows: Vec<RowModel>,
    flat_paths: Vec<PathBuf>,
    row_offsets: Vec<usize>,
}

fn read_media_dimensions(path: &Path) -> (u32, u32) {
    if let Ok(reader) = image::ImageReader::open(path) {
        if let Ok(dims) = reader.into_dimensions() {
            return dims;
        }
    }
    (1920, 1080)
}

fn scan_directory(path: &Path, viewport_width: f32) -> DirectoryScanResult {
    let mut folders: Vec<MediaItem> = Vec::new();
    let mut media_items: Vec<MediaItem> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let file_path = entry.path();

            if file_path.is_dir() {
                let name = file_path.file_name().and_then(|n| n.to_str());
                if let Some(name) = name {
                    if !name.starts_with('.') {
                        folders.push(MediaItem::new_folder(file_path));
                    }
                }
            } else if file_path.is_file() {
                let ext = file_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s.to_string());

                if let Some(ext) = ext {
                    if let Some(media_type) = MediaType::from_extension(&ext) {
                        let (width, height) = read_media_dimensions(&file_path);
                        let mut item = MediaItem::new(file_path, width, height);
                        item.media_type = media_type;
                        media_items.push(item);
                    }
                }
            }
        }
    }

    folders.sort_by(|a, b| a.path.cmp(&b.path));
    media_items.sort_by(|a, b| a.path.cmp(&b.path));

    let mut items: Vec<MediaItem> = Vec::with_capacity(folders.len() + media_items.len());
    items.extend(folders);
    items.extend(media_items);

    let rows = JustifiedLayout::default().compute(&items, viewport_width);
    let mut flat_paths = Vec::new();
    let mut row_offsets = Vec::new();
    for row in &rows {
        row_offsets.push(flat_paths.len());
        for item in &row.items {
            flat_paths.push(item.media_path.clone());
        }
    }

    DirectoryScanResult {
        items,
        rows,
        flat_paths,
        row_offsets,
    }
}

/// CSS for terminal aesthetic - embedded as fallback
const FALLBACK_CSS: &str = r#"
* {
    border-radius: 0;
    box-shadow: none;
    background-image: none;
}

window {
    background-color: #0a0a0a;
    color: #e0e0e0;
}

button {
    background-color: transparent;
    border: 1px solid #333333;
    color: #e0e0e0;
}

button:hover {
    background-color: rgba(224, 224, 224, 0.05);
    border-color: #555555;
}

.media-row {
    background-color: #0a0a0a;
    padding: 1px;
}

.media-item {
    background-color: #121212;
    border: 1px solid #333333;
    margin: 1px;
}

.media-item:hover {
    border-color: #555555;
}

.media-item.selected {
    border-color: #00ff88;
    border-style: dashed;
}

.folder-name {
    background-color: rgba(0, 0, 0, 0.7);
    color: #00ff88;
    padding: 4px 8px;
    font-size: 11px;
    font-weight: bold;
}
"#;

/// Load and apply CSS stylesheet for terminal aesthetic
fn load_css() {
    let provider = CssProvider::new();

    // Try to load from file first, fall back to embedded CSS
    let css_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/style.css");

    if Path::new(css_path).exists() {
        provider.load_from_path(css_path);
        tracing::info!("Loaded CSS from: {}", css_path);
    } else {
        // Fall back to embedded CSS
        provider.load_from_string(FALLBACK_CSS);
        tracing::info!("Loaded fallback embedded CSS");
    }

    // Apply to the default display
    if let Some(display) = Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Main window for the media browser
pub struct MainWindow {
    self_weak: RefCell<Weak<MainWindow>>,
    window: ApplicationWindow,
    stack: Stack,
    list_view: Rc<MediaListView>,
    viewer: Rc<MediaViewer>,
    keybindings: Rc<Keybindings>,
    current_path: RefCell<Option<PathBuf>>,
    media_items: RefCell<Vec<MediaItem>>,
    media_dims: RefCell<HashMap<PathBuf, (u32, u32)>>,
    folder_paths: RefCell<std::collections::HashSet<PathBuf>>,
    flat_paths: RefCell<Vec<PathBuf>>,
    row_offsets: RefCell<Vec<usize>>,
    status_label: Label,
    dir_label: Label,
    parent_button: Button,
    ui_visible: Cell<bool>,
    scan_generation: Cell<u64>,
}

impl MainWindow {
    fn effective_layout_width(
        viewport_width: f32,
        list_alloc: i32,
        window_alloc: i32,
        fallback: f32,
    ) -> f32 {
        let mut width = if viewport_width > 100.0 {
            viewport_width
        } else {
            fallback
        };
        let cap = if list_alloc > 0 {
            list_alloc as f32
        } else if window_alloc > 0 {
            window_alloc as f32
        } else {
            0.0
        };
        if cap > 0.0 {
            width = width.min(cap);
        }
        width
    }

    pub fn new(app: &Application, initial_path: Option<&Path>) -> Rc<Self> {
        // Load CSS before creating widgets
        load_css();

        // Create the main window
        let window = ApplicationWindow::builder()
            .application(app)
            .title("idxd - Media Browser")
            .default_width(1200)
            .default_height(800)
            .build();

        // Create a stack for switching between grid and viewer
        let stack = Stack::new();
        stack.set_transition_type(StackTransitionType::Crossfade);
        stack.set_transition_duration(150);

        // Create the main vertical layout for grid view
        let grid_box = GtkBox::new(Orientation::Vertical, 0);

        // Create directory header bar
        let dir_bar = GtkBox::new(Orientation::Horizontal, 8);
        dir_bar.add_css_class("dir-bar");
        dir_bar.set_margin_start(8);
        dir_bar.set_margin_end(8);
        dir_bar.set_margin_top(4);
        dir_bar.set_margin_bottom(4);

        let parent_button = Button::with_label("[..]");
        parent_button.set_tooltip_text(Some("Go to parent directory (Backspace)"));
        parent_button.add_css_class("btn-nav");

        let dir_label = Label::new(Some("> No directory"));
        dir_label.set_halign(gtk4::Align::Start);
        dir_label.set_hexpand(true);
        dir_label.add_css_class("dir-label");
        dir_label.set_ellipsize(gtk4::pango::EllipsizeMode::Start);

        dir_bar.append(&parent_button);
        dir_bar.append(&dir_label);

        // Create the media list view
        let list_view = Rc::new(MediaListView::new());

        // Create status bar
        let status_bar = GtkBox::new(Orientation::Horizontal, 8);
        status_bar.add_css_class("status-bar");
        status_bar.set_margin_start(8);
        status_bar.set_margin_end(8);
        status_bar.set_margin_top(4);
        status_bar.set_margin_bottom(4);

        let status_label = Label::new(Some("> Ready"));
        status_label.set_halign(gtk4::Align::Start);
        status_label.set_hexpand(true);
        status_label.add_css_class("muted");
        status_bar.append(&status_label);

        // Keybinding hints
        let hints_label = Label::new(Some(
            "[hjkl/arrows] Navigate  [Enter] View  [Esc] Back  [Space] Toggle  [f] Fullscreen",
        ));
        hints_label.set_halign(gtk4::Align::End);
        hints_label.add_css_class("nav-hint");
        status_bar.append(&hints_label);

        // Add widgets to grid box
        grid_box.append(&dir_bar);
        list_view.widget().set_vexpand(true);
        list_view.widget().set_hexpand(true);
        grid_box.append(list_view.widget());
        grid_box.append(&status_bar);

        // Create the viewer
        let viewer = Rc::new(MediaViewer::new());

        // Add views to stack
        stack.add_named(&grid_box, Some("grid"));
        stack.add_named(&viewer.widget(), Some("viewer"));
        stack.set_visible_child_name("grid");

        // Set the stack as window content
        window.set_child(Some(&stack));

        let current_path = RefCell::new(initial_path.map(|p| p.to_path_buf()));

        // Create keybindings
        let keybindings = Rc::new(Keybindings::new());

        let main_window = Rc::new(Self {
            self_weak: RefCell::new(Weak::new()),
            window,
            stack,
            list_view,
            viewer,
            keybindings,
            current_path,
            media_items: RefCell::new(Vec::new()),
            media_dims: RefCell::new(HashMap::new()),
            folder_paths: RefCell::new(std::collections::HashSet::new()),
            flat_paths: RefCell::new(Vec::new()),
            row_offsets: RefCell::new(Vec::new()),
            status_label,
            dir_label,
            parent_button: parent_button.clone(),
            ui_visible: Cell::new(true),
            scan_generation: Cell::new(0),
        });
        *main_window.self_weak.borrow_mut() = Rc::downgrade(&main_window);

        // Set up keybindings
        main_window.setup_keybindings();

        // Set up visible range callback for thumbnail loading
        main_window.setup_visible_range_callback();

        // Recompute grid rows when scrollbar visibility changes (content width changes).
        let window_weak = Rc::downgrade(&main_window);
        main_window
            .list_view
            .connect_vscrollbar_visibility_changed(move |_visible| {
                if let Some(window) = window_weak.upgrade() {
                    window.schedule_grid_relayout();
                }
            });

        // Connect parent button
        let window_weak = Rc::downgrade(&main_window);
        parent_button.connect_clicked(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.navigate_to_parent();
            }
        });

        // Recompute grid rows when fullscreen state changes.
        let window_weak = Rc::downgrade(&main_window);
        main_window.window.connect_fullscreened_notify(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.schedule_grid_relayout();
            }
        });

        // Connect item activation (mouse click)
        let window_weak = Rc::downgrade(&main_window);
        main_window
            .list_view
            .connect_item_activated(move |row, col, path| {
                if let Some(window) = window_weak.upgrade() {
                    window.keybindings.set_selection(row, col);
                    window.handle_item_activation(&path);
                }
            });

        // If we have an initial path, start loading it
        if let Some(path) = initial_path {
            main_window.load_directory(path);
        } else {
            main_window.set_status("> No directory specified. Use: idxd <path>");
        }

        main_window
    }

    /// Set up keybindings for the window
    fn setup_keybindings(self: &Rc<Self>) {
        // Attach keybindings to window
        self.keybindings.attach(&self.window);

        // Set up items-per-row lookup
        let model = self.list_view.model().clone();
        self.keybindings.set_items_per_row(move |row| {
            use super::list_view::RowModelObject;
            if let Some(obj) = model.item(row).and_downcast::<RowModelObject>() {
                if let Some(row_model) = obj.row_model() {
                    return row_model.items.len() as u32;
                }
            }
            0
        });

        // Set up path lookup
        let model = self.list_view.model().clone();
        self.keybindings.set_path_lookup(move |row, col| {
            use super::list_view::RowModelObject;
            if let Some(obj) = model.item(row).and_downcast::<RowModelObject>() {
                if let Some(row_model) = obj.row_model() {
                    if let Some(item) = row_model.items.get(col as usize) {
                        return Some(item.media_path.clone());
                    }
                }
            }
            None
        });

        // Set initial row count
        self.keybindings.set_row_count(self.list_view.row_count());

        // Connect selection changed callback
        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_selection_changed(move |row, col| {
            if let Some(window) = window_weak.upgrade() {
                window.on_selection_changed(row, col);
                window.prefetch_around_selection(row, col);
            }
        });

        // Connect open viewer callback (also handles folder navigation)
        let window_weak = Rc::downgrade(self);
        self.keybindings
            .connect_open_viewer(move |_row, _col, path| {
                if let Some(window) = window_weak.upgrade() {
                    window.handle_item_activation(&path);
                }
            });

        // Connect close viewer callback
        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_close_viewer(move || {
            if let Some(window) = window_weak.upgrade() {
                window.close_viewer();
            }
        });

        // Connect play/pause callback
        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_play_pause(move || {
            if let Some(window) = window_weak.upgrade() {
                window.toggle_play_pause();
            }
        });

        // Connect UI toggle callback
        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_toggle_ui(move || {
            if let Some(window) = window_weak.upgrade() {
                window.toggle_ui();
            }
        });

        // Connect fullscreen toggle callback
        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_toggle_fullscreen(move || {
            if let Some(window) = window_weak.upgrade() {
                window.toggle_fullscreen();
            }
        });

        // Connect viewer close callback
        let window_weak = Rc::downgrade(self);
        self.viewer.connect_close(move || {
            if let Some(window) = window_weak.upgrade() {
                window.keybindings.set_view_mode(ViewMode::Grid);
                window.stack.set_visible_child_name("grid");
                window.update_status_for_selection();
            }
        });
    }

    /// Handle selection change in grid
    fn on_selection_changed(&self, row: u32, col: u32) {
        // Update status bar
        self.set_status(&format!("> Selected: row {} col {}", row, col));

        // Scroll to make selection visible
        self.list_view.scroll_to_row(row);

        // TODO: Update visual selection indicator on the grid
    }

    /// Open the viewer for a media item
    fn open_viewer(&self, path: &Path) {
        tracing::info!("Opening viewer for: {}", path.display());

        // Load first so the viewer widget is visible/ready before stack switches pages.
        // This avoids a first-click no-op when the stack targets a hidden child.
        self.viewer.show(path, None);
        self.stack.set_visible_child_name("viewer");

        // Update status
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        self.set_status(&format!("> Viewing: {}", filename));

        let (row, col) = self.keybindings.selection();
        self.prefetch_around_selection(row, col);
    }

    fn prefetch_around_selection(&self, row: u32, col: u32) {
        let flat_paths = self.flat_paths.borrow();
        let row_offsets = self.row_offsets.borrow();
        let Some(start) = row_offsets.get(row as usize).cloned() else {
            return;
        };
        let idx = start.saturating_add(col as usize);
        if idx >= flat_paths.len() {
            return;
        }

        let start_idx = idx.saturating_sub(PREFETCH_RADIUS);
        let end_idx = (idx + PREFETCH_RADIUS + 1).min(flat_paths.len());

        let dims = self.media_dims.borrow();
        let mut items = Vec::new();
        for path in flat_paths[start_idx..end_idx].iter() {
            let kind = match dims.get(path) {
                Some((w, h)) if (*w as u64) * (*h as u64) <= SMALL_IMAGE_MAX_PIXELS => {
                    PrefetchKind::Full
                }
                _ => PrefetchKind::Preview,
            };
            items.push(PrefetchItem {
                path: path.clone(),
                kind,
            });
        }

        self.viewer.prefetch(items);
    }

    /// Close the viewer and return to grid
    fn close_viewer(&self) {
        tracing::info!("Closing viewer");

        self.viewer.hide();
        self.stack.set_visible_child_name("grid");
        self.update_status_for_selection();
    }

    /// Toggle play/pause for video
    fn toggle_play_pause(&self) {
        if self.viewer.is_visible() && self.viewer.is_video_mode() {
            self.viewer.toggle_video_play_pause();
        }
    }

    /// Toggle UI visibility
    fn toggle_ui(&self) {
        let visible = !self.ui_visible.get();
        self.ui_visible.set(visible);
        tracing::debug!("Toggle UI visibility: {}", visible);
        // TODO: Hide/show status bar and other UI elements
    }

    /// Toggle fullscreen mode for the app window
    fn toggle_fullscreen(&self) {
        if self.window.is_fullscreen() {
            self.window.unfullscreen();
            self.set_status("> Fullscreen: OFF");
        } else {
            self.window.fullscreen();
            self.set_status("> Fullscreen: ON");
        }
    }

    fn schedule_grid_relayout(&self) {
        let weak_self = self.self_weak.borrow().clone();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            if let Some(window) = weak_self.upgrade() {
                window.recalculate_grid_layout();
            }
            glib::ControlFlow::Break
        });
    }

    fn recalculate_grid_layout(&self) {
        let items = self.media_items.borrow().clone();
        if items.is_empty() {
            return;
        }

        let viewport_width = self.list_view.content_width();
        let (list_alloc, scrolled_alloc, vscrollbar_width, vscrollbar_visible) =
            self.list_view.debug_allocations();
        let window_alloc = self.window.allocation().width();
        tracing::debug!(
            "layout-widths content={:.1} list_alloc={} scrolled_alloc={} vscrollbar={} visible={} window_alloc={} window_width={}",
            viewport_width,
            list_alloc,
            scrolled_alloc,
            vscrollbar_width,
            vscrollbar_visible,
            window_alloc,
            self.window.width()
        );
        let effective_width = Self::effective_layout_width(
            viewport_width,
            list_alloc,
            window_alloc,
            self.window.width().max(1200) as f32,
        );

        let rows = JustifiedLayout::default().compute(&items, effective_width);
        let mut flat_paths = Vec::new();
        let mut row_offsets = Vec::new();
        for row in &rows {
            row_offsets.push(flat_paths.len());
            for item in &row.items {
                flat_paths.push(item.media_path.clone());
            }
        }

        *self.flat_paths.borrow_mut() = flat_paths;
        *self.row_offsets.borrow_mut() = row_offsets;
        self.list_view.set_rows(rows);
        self.keybindings.set_row_count(self.list_view.row_count());
    }

    /// Update status bar for current selection
    fn update_status_for_selection(&self) {
        let (row, col) = self.keybindings.selection();
        self.set_status(&format!(
            "> {} rows | Selected: row {} col {}",
            self.list_view.row_count(),
            row,
            col
        ));
    }

    /// Present the window
    pub fn present(&self) {
        self.window.present();
    }

    /// Get the current directory path
    pub fn current_path(&self) -> Option<PathBuf> {
        self.current_path.borrow().clone()
    }

    /// Set the current directory path
    pub fn set_current_path(&self, path: Option<PathBuf>) {
        *self.current_path.borrow_mut() = path.clone();
        if let Some(ref p) = path {
            let dir_name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.display().to_string());
            self.window.set_title(Some(&format!("idxd - {}", dir_name)));
            self.dir_label.set_text(&format!("> {}", p.display()));

            // Enable/disable parent button based on whether we have a parent
            self.parent_button.set_sensitive(p.parent().is_some());
        } else {
            self.dir_label.set_text("> No directory");
            self.parent_button.set_sensitive(false);
        }
    }

    /// Set status bar text
    pub fn set_status(&self, status: &str) {
        self.status_label.set_text(status);
    }

    /// Set up callback for when visible rows change
    fn setup_visible_range_callback(&self) {
        self.list_view
            .connect_visible_range_changed(move |first, last| {
                // This will be used to trigger thumbnail loading for visible rows
                tracing::debug!("Visible range: {} - {}", first, last);
            });
    }

    /// Load a directory and display its media files
    pub fn load_directory(&self, path: &Path) {
        self.set_status(&format!("> Scanning: {}", path.display()));
        self.set_current_path(Some(path.to_path_buf()));
        let viewport_width = self.list_view.content_width();
        let (list_alloc, scrolled_alloc, vscrollbar_width, vscrollbar_visible) =
            self.list_view.debug_allocations();
        let window_alloc = self.window.allocation().width();
        tracing::debug!(
            "layout-widths content={:.1} list_alloc={} scrolled_alloc={} vscrollbar={} visible={} window_alloc={} window_width={}",
            viewport_width,
            list_alloc,
            scrolled_alloc,
            vscrollbar_width,
            vscrollbar_visible,
            window_alloc,
            self.window.width()
        );
        let effective_width =
            Self::effective_layout_width(viewport_width, list_alloc, window_alloc, 1200.0);
        let generation = self.scan_generation.get().wrapping_add(1);
        self.scan_generation.set(generation);

        let path_buf = path.to_path_buf();
        let (tx, rx) = mpsc::channel::<(u64, DirectoryScanResult)>();
        std::thread::spawn(move || {
            let scanned = scan_directory(&path_buf, effective_width);
            let _ = tx.send((generation, scanned));
        });
        let requested_path = path.to_path_buf();
        let weak_self = self.self_weak.borrow().clone();

        glib::timeout_add_local(Duration::from_millis(16), move || match rx.try_recv() {
            Ok((result_generation, result)) => {
                if let Some(window) = weak_self.upgrade() {
                    window.apply_directory_scan_result(&requested_path, result_generation, result);
                }
                glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        });
    }

    fn apply_directory_scan_result(
        &self,
        requested_path: &Path,
        result_generation: u64,
        result: DirectoryScanResult,
    ) {
        if result_generation != self.scan_generation.get() {
            return;
        }
        let current = self.current_path.borrow().clone();
        if current.as_deref() != Some(requested_path) {
            return;
        }

        *self.media_items.borrow_mut() = result.items.clone();
        let mut dims = HashMap::new();
        let mut folders = std::collections::HashSet::new();
        for item in &result.items {
            dims.insert(item.path.clone(), (item.width, item.height));
            if item.is_folder() {
                folders.insert(item.path.clone());
            }
        }
        *self.media_dims.borrow_mut() = dims;
        *self.folder_paths.borrow_mut() = folders;
        *self.flat_paths.borrow_mut() = result.flat_paths;
        *self.row_offsets.borrow_mut() = result.row_offsets;

        let folder_count = result.items.iter().filter(|i| i.is_folder()).count();
        let file_count = result.items.len().saturating_sub(folder_count);
        let max_row_width = result
            .rows
            .iter()
            .map(|row| row.items.iter().map(|item| item.display_w).sum::<f32>())
            .fold(0.0_f32, f32::max);
        tracing::debug!(
            "layout-debug rows={} max_row_width_px={:.1}",
            result.rows.len(),
            max_row_width
        );
        self.list_view.set_rows(result.rows);
        self.keybindings.set_row_count(self.list_view.row_count());

        self.set_status(&format!(
            "> {} folders, {} files | {} rows | [hjkl/arrows] Navigate  [Enter] Open  [Backspace] Parent",
            folder_count,
            file_count,
            self.list_view.row_count()
        ));

        if self.list_view.row_count() > 0 {
            self.prefetch_around_selection(0, 0);
        }
    }

    /// Navigate to the parent directory
    pub fn navigate_to_parent(&self) {
        if let Some(current) = self.current_path() {
            if let Some(parent) = current.parent() {
                self.load_directory(parent);
            }
        }
    }

    /// Check if a path is a folder
    pub fn is_folder_path(&self, path: &Path) -> bool {
        self.folder_paths.borrow().contains(path)
    }

    /// Navigate into a folder
    pub fn navigate_into_folder(&self, path: &Path) {
        tracing::info!("Navigating into folder: {}", path.display());
        self.load_directory(path);
    }

    /// Handle item activation - either open viewer for media or navigate for folders
    fn handle_item_activation(&self, path: &Path) {
        if self.is_folder_path(path) {
            self.navigate_into_folder(path);
        } else {
            self.keybindings.set_view_mode(ViewMode::Viewer);
            self.open_viewer(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fallback_css_parses() {
        // Verify the fallback CSS is valid
        // This doesn't require GTK initialization
        assert!(!FALLBACK_CSS.is_empty());
        assert!(FALLBACK_CSS.contains("border-radius: 0"));
    }

    #[test]
    fn effective_layout_width_prefers_viewport_but_clamps_to_list_alloc() {
        let width = MainWindow::effective_layout_width(1400.0, 1200, 1280, 1200.0);
        assert!((width - 1200.0).abs() < 0.01);
    }

    #[test]
    fn effective_layout_width_clamps_to_window_alloc_when_list_alloc_missing() {
        let width = MainWindow::effective_layout_width(1400.0, 0, 1280, 1200.0);
        assert!((width - 1280.0).abs() < 0.01);
    }

    #[test]
    fn effective_layout_width_uses_fallback_when_viewport_uninitialized() {
        let width = MainWindow::effective_layout_width(0.0, 0, 0, 1200.0);
        assert!((width - 1200.0).abs() < 0.01);
    }

    #[test]
    fn effective_layout_width_fallback_is_still_clamped() {
        let width = MainWindow::effective_layout_width(0.0, 1100, 0, 1200.0);
        assert!((width - 1100.0).abs() < 0.01);
    }
}
