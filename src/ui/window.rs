// Main window for idxd media browser
// GTK4 ApplicationWindow with ListView, Viewer, and terminal aesthetic CSS

use gdk4::{Display, Rectangle};
use gtk4::graphene;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CheckButton, CssProvider, Entry,
    Label, Orientation, Paned, ScrolledWindow, Settings, Stack, StackTransitionType, Window,
    STYLE_PROVIDER_PRIORITY_APPLICATION,
};
use std::cell::{Cell, RefCell};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::runtime::Builder as TokioRuntimeBuilder;
use walkdir::WalkDir;

use super::keybindings::{Keybindings, ViewMode};
use super::list_view::MediaListView;
use super::row_widget::{cached_row_preview_texture, reschedule_row_previews};
use super::viewer::{MediaViewer, PrefetchItem, PrefetchKind};
use crate::layout::JustifiedLayout;
use crate::models::{MediaItem, MediaStore, MediaType, RowModel};
use crate::scanner::file_scanner::{FileScanner, ScanConfig};

const SMALL_IMAGE_MAX_PIXELS: u64 = 2_000_000;
const PREFETCH_RADIUS: usize = 24;
const PREFETCH_PRIORITY_COUNT: usize = 8;
const DIALOG_MARGIN: i32 = 12;
const SIDEBAR_WIDTH_RATIO: f32 = 0.20;
const SIDEBAR_FALLBACK_WIDTH_PX: i32 = 240;

fn default_home_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

struct DirectoryScanResult {
    items: Vec<MediaItem>,
}

struct DialogShell {
    dialog: Window,
    content: GtkBox,
    close_button: Button,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ContentSource {
    Directory,
    Favorites,
    Album { id: i64, name: String },
}

fn read_media_dimensions(path: &Path) -> (u32, u32) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    match MediaType::from_extension(ext) {
        Some(MediaType::Video) => (0, 0),
        _ => crate::image_loader::read_dimensions(path).unwrap_or((1920, 1080)),
    }
}

fn scan_directory(path: &Path, recursive: bool) -> DirectoryScanResult {
    let mut folders: Vec<MediaItem> = Vec::new();
    let mut media_items: Vec<MediaItem> = Vec::new();

    let mut add_media_file_fallback = |file_path: PathBuf| {
        let ext = file_path.extension().and_then(|e| e.to_str());
        if let Some(media_type) = ext.and_then(MediaType::from_extension) {
            let (width, height) = read_media_dimensions(&file_path);
            let mut item = MediaItem::new(file_path, width, height);
            item.media_type = media_type;
            media_items.push(item);
        }
    };

    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let file_path = entry.path();
            if file_path.is_dir() {
                if !recursive {
                    let name = file_path.file_name().and_then(|n| n.to_str());
                    if name.is_some_and(|name| !name.starts_with('.')) {
                        folders.push(MediaItem::new_folder(file_path));
                    }
                }
            }
        }
    }

    let scanner_result = (|| -> anyhow::Result<Vec<MediaItem>> {
        let store = MediaStore::open_default()?;
        let scanner = FileScanner::with_config(ScanConfig {
            recursive,
            ..ScanConfig::default()
        });
        let runtime = TokioRuntimeBuilder::new_current_thread()
            .enable_all()
            .build()?;
        let (items, _) = runtime.block_on(scanner.scan_directory(path, store))?;
        Ok(items)
    })();

    match scanner_result {
        Ok(items) => {
            media_items = items;
        }
        Err(err) => {
            tracing::warn!(error = ?err, "Scanner failed, using fallback directory walk");
            let walker = if recursive {
                WalkDir::new(path).follow_links(false).into_iter()
            } else {
                WalkDir::new(path)
                    .follow_links(false)
                    .max_depth(1)
                    .into_iter()
            };
            for entry in walker.filter_entry(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| !name.starts_with('.'))
                    .unwrap_or(true)
            }) {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => continue,
                };
                if entry.file_type().is_file() {
                    add_media_file_fallback(entry.path().to_path_buf());
                }
            }
        }
    }

    folders.sort_by(|a, b| a.path.cmp(&b.path));
    media_items.sort_by(|a, b| a.path.cmp(&b.path));

    let mut items: Vec<MediaItem> = Vec::with_capacity(folders.len() + media_items.len());
    items.extend(folders);
    items.extend(media_items);

    DirectoryScanResult { items }
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
    padding: 0;
}

.media-item {
    background-color: #121212;
    border: 1px solid #333333;
    margin: 0;
}

.media-item:hover {
    border-color: #555555;
}

.media-item.selected {
    border-color: #00ff88;
    border-style: dashed;
    border-width: 2px;
    background-color: rgba(0, 255, 136, 0.08);
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
    folder_paths: RefCell<HashSet<PathBuf>>,
    flat_paths: RefCell<Vec<PathBuf>>,
    row_offsets: RefCell<Vec<usize>>,
    status_label: Label,
    dir_label: Label,
    parent_button: Button,
    dir_bar: GtkBox,
    sidebar_scroller: ScrolledWindow,
    sidebar_items: GtkBox,
    sidebar_buttons: RefCell<Vec<(ContentSource, Button)>>,
    sidebar_toggle_button: Button,
    body_paned: Paned,
    sidebar_visible: Cell<bool>,
    sidebar_width_px: Cell<i32>,
    sidebar_width_initialized: Cell<bool>,
    active_source: RefCell<ContentSource>,
    status_bar: GtkBox,
    shuffle_button: Button,
    ui_visible: Cell<bool>,
    last_layout_width: Cell<i32>,
    resize_relayout_pending: Cell<bool>,
    scan_generation: Cell<u64>,
    recursive_scan: Cell<bool>,
    prefer_dark: Cell<bool>,
    shuffle_mode: Cell<bool>,
    shuffle_seed: Cell<u64>,
    base_items: RefCell<Vec<MediaItem>>,
    media_store: RefCell<Option<MediaStore>>,
    pending_viewer_target: RefCell<Option<PathBuf>>,
    grid_scroll_before_viewer: Cell<f64>,
}

impl MainWindow {
    fn sanitize_positive_width(value: f32) -> Option<f32> {
        if value.is_finite() && value > 0.0 {
            Some(value.max(1.0))
        } else {
            None
        }
    }

    fn build_dialog_shell(&self, title: &str, width: i32) -> DialogShell {
        let dialog = Window::builder()
            .title(title)
            .transient_for(&self.window)
            .modal(true)
            .resizable(false)
            .default_width(width)
            .build();

        let content = GtkBox::new(Orientation::Vertical, 12);
        content.set_margin_top(DIALOG_MARGIN);
        content.set_margin_bottom(DIALOG_MARGIN);
        content.set_margin_start(DIALOG_MARGIN);
        content.set_margin_end(DIALOG_MARGIN);

        let header = GtkBox::new(Orientation::Horizontal, 8);
        let close_button = Button::with_label("Close");
        header.append(&close_button);
        let header_spacer = GtkBox::new(Orientation::Horizontal, 0);
        header_spacer.set_hexpand(true);
        header.append(&header_spacer);
        content.append(&header);

        dialog.set_child(Some(&content));

        DialogShell {
            dialog,
            content,
            close_button,
        }
    }

    fn effective_layout_width(
        viewport_width: f32,
        cap_alloc: i32,
        window_alloc: i32,
        fallback: f32,
    ) -> f32 {
        let chosen_width = if viewport_width > 100.0 && viewport_width.is_finite() {
            viewport_width
        } else {
            fallback
        };

        let mut width = Self::sanitize_positive_width(chosen_width).unwrap_or(1.0);

        let cap = match (cap_alloc > 0, window_alloc > 0) {
            (true, true) => Some((cap_alloc.min(window_alloc)) as f32),
            (true, false) => Some(cap_alloc as f32),
            (false, true) => Some(window_alloc as f32),
            (false, false) => None,
        }
        .and_then(Self::sanitize_positive_width);

        if let Some(cap) = cap {
            width = width.min(cap);
        }

        width.max(1.0)
    }

    fn clamp_rows_to_width(mut rows: Vec<RowModel>, max_width: f32) -> Vec<RowModel> {
        let Some(max_width) = Self::sanitize_positive_width(max_width) else {
            return rows;
        };

        for row in &mut rows {
            if row.items.is_empty() {
                row.height_px = Self::sanitize_positive_width(row.height_px).unwrap_or(1.0);
                continue;
            }

            let mut total_width = 0.0f32;
            let mut row_height = 1.0f32;

            for item in &mut row.items {
                item.display_w = Self::sanitize_positive_width(item.display_w).unwrap_or(1.0);
                item.display_h = Self::sanitize_positive_width(item.display_h).unwrap_or(1.0);
                if !item.offset_top.is_finite() || item.offset_top < 0.0 {
                    item.offset_top = 0.0;
                }
                total_width += item.display_w;
                row_height = row_height.max(item.display_h + item.offset_top);
            }

            if total_width > max_width {
                let scale = (max_width / total_width).clamp(0.0, 1.0);
                row_height = 1.0;
                for item in &mut row.items {
                    item.display_w = (item.display_w * scale).max(1.0);
                    item.display_h = (item.display_h * scale).max(1.0);
                    item.offset_top = (item.offset_top * scale).max(0.0);
                    row_height = row_height.max(item.display_h + item.offset_top);
                }
            }

            row.height_px = row_height.max(1.0);
        }

        rows
    }

    pub fn new(app: &Application, initial_path: Option<&Path>) -> Rc<Self> {
        // Load CSS before creating widgets
        load_css();
        if let Some(settings) = Settings::default() {
            settings.set_gtk_application_prefer_dark_theme(true);
        }

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

        // Create sidebar for quick source switching
        let sidebar_items = GtkBox::new(Orientation::Vertical, 4);
        sidebar_items.add_css_class("sidebar-items");

        let sidebar_content = GtkBox::new(Orientation::Vertical, 6);
        sidebar_content.add_css_class("sidebar-panel");
        sidebar_content.set_margin_start(8);
        sidebar_content.set_margin_end(8);
        sidebar_content.set_margin_top(6);
        sidebar_content.set_margin_bottom(6);

        let sidebar_title = Label::new(Some("Library"));
        sidebar_title.set_halign(Align::Start);
        sidebar_title.add_css_class("section-title");
        sidebar_content.append(&sidebar_title);
        sidebar_content.append(&sidebar_items);

        let sidebar_scroller = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .propagate_natural_width(true)
            .propagate_natural_height(false)
            .child(&sidebar_content)
            .build();
        sidebar_scroller.add_css_class("sidebar-scroll");

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
            "[hjkl/arrows] Navigate  [Enter] View  [Esc] Back  [Space] UI  [f] Fullscreen  [o] Open  [r] Recursive  [s] Shuffle  [+] Favorite  [Del] Delete  [Right Click] Album",
        ));
        hints_label.set_halign(gtk4::Align::End);
        hints_label.add_css_class("nav-hint");
        status_bar.append(&hints_label);

        let shuffle_button = Button::with_label("S/Shuffle: OFF");
        shuffle_button.set_tooltip_text(Some("Shuffle items (S)"));
        shuffle_button.add_css_class("btn-nav");
        status_bar.append(&shuffle_button);

        let body_paned = Paned::new(Orientation::Horizontal);
        body_paned.set_vexpand(true);
        body_paned.set_hexpand(true);
        body_paned.set_wide_handle(true);
        body_paned.set_start_child(Some(&sidebar_scroller));
        list_view.widget().set_vexpand(true);
        list_view.widget().set_hexpand(true);
        body_paned.set_end_child(Some(list_view.widget()));

        // Add widgets to grid box
        grid_box.append(&dir_bar);
        grid_box.append(&body_paned);
        grid_box.append(&status_bar);

        // Create the viewer
        let viewer = Rc::new(MediaViewer::new());

        // Add views to stack
        stack.add_named(&grid_box, Some("grid"));
        stack.add_named(&viewer.widget(), Some("viewer"));
        stack.set_visible_child_name("grid");

        // Set the stack as window content
        window.set_child(Some(&stack));

        let resolved_initial_path = initial_path
            .map(|p| p.to_path_buf())
            .or_else(default_home_dir);
        let current_path = RefCell::new(resolved_initial_path.clone());

        let sidebar_toggle_button = Button::with_label("[sidebar: ON]");
        sidebar_toggle_button.set_tooltip_text(Some("Toggle sidebar"));
        sidebar_toggle_button.add_css_class("btn-nav");

        let settings_button = Button::with_label("[settings]");
        settings_button.set_tooltip_text(Some("Settings"));
        settings_button.add_css_class("btn-nav");
        let dir_spacer = GtkBox::new(Orientation::Horizontal, 0);
        dir_spacer.set_hexpand(true);
        dir_bar.append(&dir_spacer);
        dir_bar.append(&sidebar_toggle_button);
        dir_bar.append(&settings_button);

        // Create keybindings
        let keybindings = Rc::new(Keybindings::new());

        let media_store = match MediaStore::open_default() {
            Ok(store) => Some(store),
            Err(err) => {
                tracing::warn!(error = ?err, "Failed to open media store");
                None
            }
        };

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
            folder_paths: RefCell::new(HashSet::new()),
            flat_paths: RefCell::new(Vec::new()),
            row_offsets: RefCell::new(Vec::new()),
            status_label,
            dir_label,
            parent_button: parent_button.clone(),
            dir_bar: dir_bar.clone(),
            sidebar_scroller: sidebar_scroller.clone(),
            sidebar_items: sidebar_items.clone(),
            sidebar_buttons: RefCell::new(Vec::new()),
            sidebar_toggle_button: sidebar_toggle_button.clone(),
            body_paned: body_paned.clone(),
            sidebar_visible: Cell::new(true),
            sidebar_width_px: Cell::new(SIDEBAR_FALLBACK_WIDTH_PX),
            sidebar_width_initialized: Cell::new(false),
            active_source: RefCell::new(ContentSource::Directory),
            status_bar: status_bar.clone(),
            shuffle_button: shuffle_button.clone(),
            ui_visible: Cell::new(true),
            last_layout_width: Cell::new(0),
            resize_relayout_pending: Cell::new(false),
            scan_generation: Cell::new(0),
            recursive_scan: Cell::new(false),
            prefer_dark: Cell::new(true),
            shuffle_mode: Cell::new(false),
            shuffle_seed: Cell::new(0),
            base_items: RefCell::new(Vec::new()),
            media_store: RefCell::new(media_store),
            pending_viewer_target: RefCell::new(None),
            grid_scroll_before_viewer: Cell::new(0.0),
        });
        *main_window.self_weak.borrow_mut() = Rc::downgrade(&main_window);
        main_window.rebuild_sidebar();

        // Set up keybindings
        main_window.setup_keybindings();

        // Set up visible range callback for thumbnail loading
        main_window.setup_visible_range_callback();
        main_window.setup_layout_resize_observer();

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

        // Connect settings button
        let window_weak = Rc::downgrade(&main_window);
        settings_button.connect_clicked(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.open_settings();
            }
        });

        let window_weak = Rc::downgrade(&main_window);
        shuffle_button.connect_clicked(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.toggle_shuffle();
            }
        });

        let window_weak = Rc::downgrade(&main_window);
        sidebar_toggle_button.connect_clicked(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.toggle_sidebar();
            }
        });

        let window_weak = Rc::downgrade(&main_window);
        body_paned.connect_position_notify(move |paned| {
            if let Some(window) = window_weak.upgrade() {
                if !window.sidebar_visible.get() {
                    return;
                }
                let position = paned.position();
                let clamped = window.clamp_sidebar_width(position);
                window.sidebar_width_px.set(clamped);
                if clamped != position {
                    paned.set_position(clamped);
                }
                window.schedule_grid_relayout_debounced(Duration::from_millis(40));
            }
        });

        main_window.update_sidebar_toggle_button();
        main_window.refresh_sidebar_layout(true);

        // Recompute grid rows when fullscreen state changes.
        let window_weak = Rc::downgrade(&main_window);
        main_window.window.connect_fullscreened_notify(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.schedule_grid_relayout_after(Duration::from_millis(50));
                window.schedule_grid_relayout_after(Duration::from_millis(200));
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

        let window_weak = Rc::downgrade(&main_window);
        main_window
            .list_view
            .connect_item_context_menu(move |row, col, path, widget, rect| {
                if let Some(window) = window_weak.upgrade() {
                    window.keybindings.set_selection(row, col);
                    window.show_album_menu(path, &widget, rect);
                }
            });

        // If we have an initial path, start loading it
        if let Some(path) = resolved_initial_path.as_deref() {
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

        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_rotate_ccw(move || {
            if let Some(window) = window_weak.upgrade() {
                window.viewer.rotate_current_relative(-1);
            }
        });

        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_rotate_cw(move || {
            if let Some(window) = window_weak.upgrade() {
                window.viewer.rotate_current_relative(1);
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

        // Connect recursive toggle callback
        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_toggle_recursive(move || {
            if let Some(window) = window_weak.upgrade() {
                let next = !window.recursive_scan.get();
                window.set_recursive_scan(next);
                window.set_status(&format!(
                    "> Recursive scan: {}",
                    if next { "ON" } else { "OFF" }
                ));
            }
        });

        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_toggle_shuffle(move || {
            if let Some(window) = window_weak.upgrade() {
                window.toggle_shuffle();
            }
        });

        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_toggle_favorite(move || {
            if let Some(window) = window_weak.upgrade() {
                window.toggle_favorite_selected();
            }
        });

        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_delete_selected(move || {
            if let Some(window) = window_weak.upgrade() {
                window.delete_selected();
            }
        });

        // Connect open directory prompt callback
        let window_weak = Rc::downgrade(self);
        self.keybindings.connect_open_directory(move || {
            if let Some(window) = window_weak.upgrade() {
                window.prompt_open_directory();
            }
        });

        // Connect viewer close callback
        let window_weak = Rc::downgrade(self);
        self.viewer.connect_close(move || {
            if let Some(window) = window_weak.upgrade() {
                window.close_viewer();
            }
        });

        let window_weak = Rc::downgrade(self);
        self.viewer.connect_context_menu(move |path, widget, rect| {
            if let Some(window) = window_weak.upgrade() {
                window.show_album_menu(path, &widget, rect);
            }
        });

        let window_weak = Rc::downgrade(self);
        self.viewer.connect_toggle_favorite(move || {
            if let Some(window) = window_weak.upgrade() {
                if let Some(path) = window.viewer.current_path() {
                    if let Some(state) = window.toggle_favorite_for_path(&path) {
                        window.viewer.set_favorite_state(state);
                    }
                }
            }
        });
    }

    /// Handle selection change in grid
    fn on_selection_changed(&self, row: u32, col: u32) {
        // Update status bar
        self.set_status(&format!("> Selected: row {} col {}", row, col));

        // Scroll to make selection visible
        self.list_view.scroll_to_row(row);
        self.list_view.set_selection(row, col);
    }

    /// Open the viewer for a media item
    fn open_viewer(&self, path: &Path) {
        tracing::info!("Opening viewer for: {}", path.display());
        self.grid_scroll_before_viewer
            .set(self.list_view.scroll_value());
        if let Some(texture) = cached_row_preview_texture(path) {
            let (mut orig_w, mut orig_h) = self
                .media_dims
                .borrow()
                .get(path)
                .copied()
                .unwrap_or((0, 0));
            if orig_w == 0 || orig_h == 0 {
                if let Ok((w, h)) = crate::image_loader::read_dimensions(path) {
                    orig_w = w;
                    orig_h = h;
                }
            }
            if orig_w == 0 || orig_h == 0 {
                orig_w = texture.width().max(1) as u32;
                orig_h = texture.height().max(1) as u32;
            }
            self.viewer
                .prime_preview_texture(path, &texture, orig_w, orig_h);
        }
        let thumbnail_path = self.thumbnail_path_for(path);

        // Load first so the viewer widget is visible/ready before stack switches pages.
        // This avoids a first-click no-op when the stack targets a hidden child.
        self.viewer.show(path, thumbnail_path.as_deref());
        self.viewer
            .set_favorite_state(self.favorite_state_for_path(path).unwrap_or(false));
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

    fn thumbnail_path_for(&self, path: &Path) -> Option<PathBuf> {
        if let Some(item_thumb) = self
            .media_items
            .borrow()
            .iter()
            .find(|item| item.path == path)
            .and_then(|item| item.thumb_path.clone())
            .filter(|thumb_path| thumb_path.exists())
        {
            return Some(item_thumb);
        }

        let store_ref = self.media_store.borrow();
        let store = store_ref.as_ref()?;
        match store.get_media(path) {
            Ok(Some(item)) => item.thumb_path.filter(|thumb_path| thumb_path.exists()),
            Ok(None) => None,
            Err(err) => {
                tracing::debug!(
                    error = ?err,
                    path = %path.display(),
                    "Failed to resolve thumbnail path for viewer open"
                );
                None
            }
        }
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

        let folders = self.folder_paths.borrow();
        let mut ordered_indices: Vec<usize> = (start_idx..end_idx)
            .filter(|i| !folders.contains(&flat_paths[*i]))
            .collect();
        ordered_indices.sort_by_key(|i| i.abs_diff(idx));

        let mut items = Vec::new();
        for index in ordered_indices.into_iter().take(PREFETCH_PRIORITY_COUNT) {
            items.push(PrefetchItem {
                path: flat_paths[index].clone(),
                kind: PrefetchKind::Preview,
            });
        }

        self.viewer.prefetch(items);
    }

    /// Close the viewer and return to grid
    fn close_viewer(&self) {
        tracing::info!("Closing viewer");

        self.viewer.hide();
        self.stack.set_visible_child_name("grid");
        let target_scroll = self.grid_scroll_before_viewer.get();
        self.list_view.set_scroll_value(target_scroll);
        let list_view = self.list_view.clone();
        glib::idle_add_local_once(move || {
            list_view.set_scroll_value(target_scroll);
        });
        let list_view = self.list_view.clone();
        glib::timeout_add_local_once(Duration::from_millis(32), move || {
            list_view.set_scroll_value(target_scroll);
        });
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
        self.set_ui_visible(visible);
        tracing::debug!("Toggle UI visibility: {}", visible);
    }

    fn set_ui_visible(&self, visible: bool) {
        self.ui_visible.set(visible);
        self.dir_bar.set_visible(visible);
        self.status_bar.set_visible(visible);
    }

    fn sidebar_width_cap_px(&self) -> i32 {
        let window_width = self.window.width();
        if window_width > 0 {
            ((window_width as f32) * SIDEBAR_WIDTH_RATIO).round() as i32
        } else {
            SIDEBAR_FALLBACK_WIDTH_PX
        }
        .max(1)
    }

    fn clamp_sidebar_width(&self, width: i32) -> i32 {
        let cap = self.sidebar_width_cap_px().max(1);
        if width <= 0 {
            cap
        } else {
            width.min(cap).max(1)
        }
    }

    fn refresh_sidebar_layout(&self, apply_default: bool) {
        let cap = self.sidebar_width_cap_px().max(1);
        self.sidebar_scroller.set_max_content_width(cap);

        if apply_default && !self.sidebar_width_initialized.get() {
            self.sidebar_width_px.set(cap);
            self.sidebar_width_initialized.set(true);
        } else {
            let clamped = self.clamp_sidebar_width(self.sidebar_width_px.get());
            self.sidebar_width_px.set(clamped);
        }

        if self.sidebar_visible.get() {
            let current = self.body_paned.position();
            let target = if current > 0 {
                current
            } else {
                self.sidebar_width_px.get()
            };
            let clamped = self.clamp_sidebar_width(target);
            self.sidebar_width_px.set(clamped);
            if clamped != current {
                self.body_paned.set_position(clamped);
            }
        }
    }

    fn update_sidebar_toggle_button(&self) {
        let label = if self.sidebar_visible.get() {
            "[sidebar: ON]"
        } else {
            "[sidebar: OFF]"
        };
        self.sidebar_toggle_button.set_label(label);
    }

    fn toggle_sidebar(&self) {
        let visible = !self.sidebar_visible.get();
        self.set_sidebar_visible(visible);
    }

    fn set_sidebar_visible(&self, visible: bool) {
        if self.sidebar_visible.get() == visible {
            return;
        }

        if visible {
            self.sidebar_scroller.set_visible(true);
            self.refresh_sidebar_layout(false);
            let width = self.clamp_sidebar_width(self.sidebar_width_px.get());
            self.sidebar_width_px.set(width);
            self.body_paned.set_position(width);
        } else {
            let current = self.body_paned.position();
            if current > 0 {
                self.sidebar_width_px.set(self.clamp_sidebar_width(current));
            }
            self.sidebar_scroller.set_visible(false);
            self.body_paned.set_position(0);
        }

        self.sidebar_visible.set(visible);
        self.update_sidebar_toggle_button();
        self.schedule_grid_relayout();
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

    fn prompt_open_directory(&self) {
        let dialog_shell = self.build_dialog_shell("Open directory", 520);
        let dialog = dialog_shell.dialog;
        let content = dialog_shell.content;
        let close_button = dialog_shell.close_button;

        let entry = Entry::new();
        entry.set_hexpand(true);
        entry.set_placeholder_text(Some("/path/to/folder"));
        if let Some(current) = self.current_path() {
            entry.set_text(current.to_string_lossy().as_ref());
            entry.select_region(0, -1);
        }
        content.append(&entry);

        let buttons = GtkBox::new(Orientation::Horizontal, 8);
        buttons.set_halign(Align::End);
        let cancel_button = Button::with_label("Cancel");
        let open_button = Button::with_label("Open");
        buttons.append(&cancel_button);
        buttons.append(&open_button);
        content.append(&buttons);

        let dialog_weak = dialog.downgrade();
        let close_dialog = Rc::new(move || {
            if let Some(dialog) = dialog_weak.upgrade() {
                dialog.close();
            }
        });

        let window_weak = self.self_weak.borrow().clone();
        let entry_for_open = entry.clone();
        let close_dialog_for_open = close_dialog.clone();
        let open_action = Rc::new(move || {
            if let Some(window) = window_weak.upgrade() {
                let input = entry_for_open.text().to_string();
                let input = input.trim();
                if !input.is_empty() {
                    let path = window.expand_path_input(input);
                    if path.is_dir() {
                        if window.viewer.is_visible() {
                            window.close_viewer();
                        } else {
                            window.keybindings.set_view_mode(ViewMode::Grid);
                            window.stack.set_visible_child_name("grid");
                        }
                        window.load_directory(&path);
                    } else {
                        window.set_status(&format!("> Not a directory: {}", path.display()));
                    }
                }
            }
            close_dialog_for_open();
        });

        let open_action_for_button = open_action.clone();
        open_button.connect_clicked(move |_| {
            open_action_for_button();
        });

        let open_action_for_entry = open_action.clone();
        entry.connect_activate(move |_| {
            open_action_for_entry();
        });

        let close_dialog_for_cancel = close_dialog.clone();
        cancel_button.connect_clicked(move |_| {
            close_dialog_for_cancel();
        });

        let close_dialog_for_close = close_dialog.clone();
        close_button.connect_clicked(move |_| {
            close_dialog_for_close();
        });

        dialog.set_default_widget(Some(&open_button));
        dialog.present();
        entry.grab_focus();
    }

    fn open_settings(&self) {
        let dialog_shell = self.build_dialog_shell("Settings", 420);
        let dialog = dialog_shell.dialog;
        let content = dialog_shell.content;
        let close_button = dialog_shell.close_button;

        let dark_toggle = CheckButton::with_label("Dark mode");
        dark_toggle.set_active(self.prefer_dark.get());
        content.append(&dark_toggle);

        let recursive_toggle = CheckButton::with_label("Recursive scan");
        recursive_toggle.set_active(self.recursive_scan.get());
        content.append(&recursive_toggle);

        let ui_toggle = CheckButton::with_label("Show header + status bars");
        ui_toggle.set_active(self.ui_visible.get());
        content.append(&ui_toggle);

        let sidebar_toggle = CheckButton::with_label("Show sidebar");
        sidebar_toggle.set_active(self.sidebar_visible.get());
        content.append(&sidebar_toggle);

        let window_weak = self.self_weak.borrow().clone();
        dark_toggle.connect_toggled(move |toggle| {
            if let Some(window) = window_weak.upgrade() {
                window.set_prefer_dark(toggle.is_active());
            }
        });

        let window_weak = self.self_weak.borrow().clone();
        recursive_toggle.connect_toggled(move |toggle| {
            if let Some(window) = window_weak.upgrade() {
                window.set_recursive_scan(toggle.is_active());
            }
        });

        let window_weak = self.self_weak.borrow().clone();
        ui_toggle.connect_toggled(move |toggle| {
            if let Some(window) = window_weak.upgrade() {
                window.set_ui_visible(toggle.is_active());
            }
        });

        let window_weak = self.self_weak.borrow().clone();
        sidebar_toggle.connect_toggled(move |toggle| {
            if let Some(window) = window_weak.upgrade() {
                window.set_sidebar_visible(toggle.is_active());
            }
        });

        let dialog_weak = dialog.downgrade();
        close_button.connect_clicked(move |_| {
            if let Some(dialog) = dialog_weak.upgrade() {
                dialog.close();
            }
        });

        dialog.present();
    }

    fn expand_path_input(&self, input: &str) -> PathBuf {
        if input == "~" || input.starts_with("~/") {
            if let Some(home) = default_home_dir() {
                if input == "~" {
                    return home;
                }
                let rest = input.trim_start_matches("~/");
                return home.join(rest);
            }
        }
        PathBuf::from(input)
    }

    fn schedule_grid_relayout(&self) {
        self.schedule_grid_relayout_after(Duration::from_millis(50));
    }

    fn schedule_grid_relayout_after(&self, delay: Duration) {
        let weak_self = self.self_weak.borrow().clone();
        glib::timeout_add_local(delay, move || {
            if let Some(window) = weak_self.upgrade() {
                window.recalculate_grid_layout();
            }
            glib::ControlFlow::Break
        });
    }

    fn schedule_grid_relayout_debounced(&self, delay: Duration) {
        if self.resize_relayout_pending.replace(true) {
            return;
        }
        let weak_self = self.self_weak.borrow().clone();
        glib::timeout_add_local(delay, move || {
            if let Some(window) = weak_self.upgrade() {
                window.resize_relayout_pending.set(false);
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
        let window_alloc = self.window.width();
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
        let rows = self.build_rows_for_items(&items);
        self.apply_rows(rows);
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

    pub fn connect_close_request<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        self.window.connect_close_request(move |_| {
            callback();
            glib::Propagation::Proceed
        });
    }

    /// Get the current directory path
    pub fn current_path(&self) -> Option<PathBuf> {
        self.current_path.borrow().clone()
    }

    fn update_source_header(&self) {
        match self.active_source.borrow().clone() {
            ContentSource::Directory => {
                let current = self.current_path.borrow().clone();
                if let Some(p) = current {
                    let dir_name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.display().to_string());
                    self.window.set_title(Some(&format!("idxd - {}", dir_name)));
                    self.dir_label.set_text(&format!("> {}", p.display()));
                    self.parent_button.set_sensitive(p.parent().is_some());
                } else {
                    self.window.set_title(Some("idxd - Media Browser"));
                    self.dir_label.set_text("> No directory");
                    self.parent_button.set_sensitive(false);
                }
            }
            ContentSource::Favorites => {
                self.window.set_title(Some("idxd - Favourites"));
                self.dir_label.set_text("> Favourites");
                self.parent_button.set_sensitive(false);
            }
            ContentSource::Album { name, .. } => {
                self.window
                    .set_title(Some(&format!("idxd - Album: {}", name)));
                self.dir_label.set_text(&format!("> Album: {}", name));
                self.parent_button.set_sensitive(false);
            }
        }
    }

    fn set_active_source(&self, source: ContentSource) {
        *self.active_source.borrow_mut() = source;
        self.update_source_header();
        self.update_sidebar_selection();
    }

    fn clear_sidebar_items(&self) {
        while let Some(child) = self.sidebar_items.first_child() {
            self.sidebar_items.remove(&child);
        }
        self.sidebar_buttons.borrow_mut().clear();
    }

    fn add_sidebar_entry(&self, label: &str, source: ContentSource, sensitive: bool) {
        let button = Button::with_label(label);
        button.set_hexpand(true);
        button.set_halign(Align::Fill);
        button.add_css_class("sidebar-entry");
        button.set_sensitive(sensitive);
        let window_weak = self.self_weak.borrow().clone();
        let source_for_click = source.clone();
        button.connect_clicked(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.switch_content_source(source_for_click.clone());
            }
        });
        self.sidebar_items.append(&button);
        self.sidebar_buttons.borrow_mut().push((source, button));
    }

    fn update_sidebar_selection(&self) {
        let active = self.active_source.borrow().clone();
        for (source, button) in self.sidebar_buttons.borrow().iter() {
            if source == &active {
                button.add_css_class("sidebar-entry-active");
            } else {
                button.remove_css_class("sidebar-entry-active");
            }
        }
    }

    fn rebuild_sidebar(&self) {
        self.clear_sidebar_items();

        self.add_sidebar_entry("Current Directory", ContentSource::Directory, true);
        let has_store = self.media_store.borrow().is_some();
        self.add_sidebar_entry("Favourites", ContentSource::Favorites, has_store);

        let sep = gtk4::Separator::new(Orientation::Horizontal);
        self.sidebar_items.append(&sep);

        let albums_label = Label::new(Some("Albums"));
        albums_label.set_halign(Align::Start);
        albums_label.add_css_class("label-small");
        self.sidebar_items.append(&albums_label);

        let store = self.media_store.borrow();
        if let Some(store) = store.as_ref() {
            match store.list_albums() {
                Ok(albums) if albums.is_empty() => {
                    let empty = Label::new(Some("No albums yet"));
                    empty.set_halign(Align::Start);
                    empty.add_css_class("muted");
                    self.sidebar_items.append(&empty);
                }
                Ok(albums) => {
                    for (album_id, name) in albums {
                        let label = name.clone();
                        self.add_sidebar_entry(
                            &label,
                            ContentSource::Album { id: album_id, name },
                            true,
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(error = ?err, "Failed to list albums for sidebar");
                    let failed = Label::new(Some("Failed to load albums"));
                    failed.set_halign(Align::Start);
                    failed.add_css_class("muted");
                    self.sidebar_items.append(&failed);
                }
            }
        } else {
            let unavailable = Label::new(Some("Albums unavailable"));
            unavailable.set_halign(Align::Start);
            unavailable.add_css_class("muted");
            self.sidebar_items.append(&unavailable);
        }

        self.update_sidebar_selection();
    }

    fn media_item_from_path(path: &Path) -> Option<MediaItem> {
        if path.is_dir() {
            return Some(MediaItem::new_folder(path.to_path_buf()));
        }
        if !path.is_file() {
            return None;
        }
        let ext = path.extension().and_then(|e| e.to_str())?;
        let media_type = MediaType::from_extension(ext)?;
        let (width, height) = read_media_dimensions(path);
        let mut item = MediaItem::new(path.to_path_buf(), width, height);
        item.media_type = media_type;
        Some(item)
    }

    fn media_items_for_paths(&self, paths: &[PathBuf]) -> Vec<MediaItem> {
        if paths.is_empty() {
            return Vec::new();
        }

        let mut cached = HashMap::new();
        if let Some(store) = self.media_store.borrow().as_ref() {
            match store.get_media_batch(paths) {
                Ok(items) => {
                    for item in items {
                        cached.insert(item.path.clone(), item);
                    }
                }
                Err(err) => {
                    tracing::warn!(error = ?err, "Failed to read cached media items");
                }
            }
        }

        let mut dedupe = HashSet::new();
        let mut items = Vec::with_capacity(paths.len());
        for path in paths {
            if !dedupe.insert(path.clone()) {
                continue;
            }
            if let Some(item) = cached.remove(path) {
                if item.path.exists() {
                    items.push(item);
                }
                continue;
            }
            if let Some(item) = Self::media_item_from_path(path) {
                items.push(item);
            }
        }
        items
    }

    fn apply_items(&self, base_items: Vec<MediaItem>) {
        *self.base_items.borrow_mut() = base_items.clone();
        let items = if self.shuffle_mode.get() {
            self.shuffled_items(&base_items)
        } else {
            base_items
        };
        *self.media_items.borrow_mut() = items.clone();

        let mut dims = HashMap::new();
        let mut folders = HashSet::new();
        for item in &items {
            dims.insert(item.path.clone(), (item.width, item.height));
            if item.is_folder() {
                folders.insert(item.path.clone());
            }
        }
        *self.media_dims.borrow_mut() = dims;
        *self.folder_paths.borrow_mut() = folders;

        let rows = self.build_rows_for_items(&items);
        self.apply_rows(rows);
        self.update_shuffle_button();
    }

    fn finalize_loaded_items(&self) {
        if let Some(target) = self.pending_viewer_target.borrow_mut().take() {
            if let Some((row, col)) = self.selection_for_path(&target) {
                self.keybindings.set_selection(row, col);
                self.keybindings.set_view_mode(ViewMode::Viewer);
                self.open_viewer(&target);
            }
        }

        if self.list_view.row_count() > 0 {
            let (row, col) = self.keybindings.selection();
            self.prefetch_around_selection(row, col);
        }
    }

    fn load_favorites(&self) {
        let paths = {
            let store_ref = self.media_store.borrow();
            let Some(store) = store_ref.as_ref() else {
                self.set_status("> Favourites unavailable (database error)");
                return;
            };
            match store.list_favorite_paths() {
                Ok(paths) => paths,
                Err(err) => {
                    tracing::warn!(error = ?err, "Failed to list favourites");
                    self.set_status("> Failed to load favourites");
                    return;
                }
            }
        };

        let items = self.media_items_for_paths(&paths);
        self.set_active_source(ContentSource::Favorites);
        self.apply_items(items);
        self.finalize_loaded_items();
        self.set_status(&format!(
            "> Favourites: {} items | {} rows | [hjkl/arrows] Navigate  [Enter] Open",
            self.media_items.borrow().len(),
            self.list_view.row_count()
        ));
    }

    fn load_album(&self, album_id: i64, name: &str) {
        let paths = {
            let store_ref = self.media_store.borrow();
            let Some(store) = store_ref.as_ref() else {
                self.set_status("> Albums unavailable (database error)");
                return;
            };
            match store.list_album_paths(album_id) {
                Ok(paths) => paths,
                Err(err) => {
                    tracing::warn!(error = ?err, "Failed to load album paths");
                    self.set_status("> Failed to load album");
                    return;
                }
            }
        };

        let items = self.media_items_for_paths(&paths);
        self.set_active_source(ContentSource::Album {
            id: album_id,
            name: name.to_string(),
        });
        self.apply_items(items);
        self.finalize_loaded_items();
        self.set_status(&format!(
            "> Album: {} | {} items | {} rows | [hjkl/arrows] Navigate  [Enter] Open",
            name,
            self.media_items.borrow().len(),
            self.list_view.row_count()
        ));
    }

    fn switch_content_source(&self, source: ContentSource) {
        match source {
            ContentSource::Directory => {
                if let Some(path) = self.current_path() {
                    self.load_directory(&path);
                } else {
                    self.set_active_source(ContentSource::Directory);
                    self.set_status("> No directory selected. Use [o] to open one");
                }
            }
            ContentSource::Favorites => {
                self.scan_generation
                    .set(self.scan_generation.get().wrapping_add(1));
                self.load_favorites();
            }
            ContentSource::Album { id, name } => {
                self.scan_generation
                    .set(self.scan_generation.get().wrapping_add(1));
                self.load_album(id, &name);
            }
        }
    }

    fn reload_active_source(&self) {
        let source = self.active_source.borrow().clone();
        self.switch_content_source(source);
    }

    /// Set the current directory path
    pub fn set_current_path(&self, path: Option<PathBuf>) {
        *self.current_path.borrow_mut() = path;
        if matches!(*self.active_source.borrow(), ContentSource::Directory) {
            self.update_source_header();
        }
    }

    /// Set status bar text
    pub fn set_status(&self, status: &str) {
        self.status_label.set_text(status);
    }

    fn set_prefer_dark(&self, enabled: bool) {
        self.prefer_dark.set(enabled);
        if let Some(settings) = Settings::default() {
            settings.set_gtk_application_prefer_dark_theme(enabled);
        }
    }

    fn rows_to_flat_paths(rows: &[RowModel]) -> (Vec<PathBuf>, Vec<usize>) {
        let mut flat_paths = Vec::new();
        let mut row_offsets = Vec::new();
        for row in rows {
            row_offsets.push(flat_paths.len());
            for item in &row.items {
                flat_paths.push(item.media_path.clone());
            }
        }
        (flat_paths, row_offsets)
    }

    fn current_effective_width(&self, fallback: f32) -> f32 {
        let viewport_width = self.list_view.content_width();
        let (_list_alloc, scrolled_alloc, _vscrollbar_width, _vscrollbar_visible) =
            self.list_view.debug_allocations();
        let window_alloc = self.window.width();
        Self::effective_layout_width(viewport_width, scrolled_alloc, window_alloc, fallback)
    }

    fn build_rows_for_items(&self, items: &[MediaItem]) -> Vec<RowModel> {
        let effective_width = self.current_effective_width(1200.0);
        let rows = JustifiedLayout::default().compute(items, effective_width);
        Self::clamp_rows_to_width(rows, effective_width)
    }

    fn apply_rows(&self, rows: Vec<RowModel>) {
        let (flat_paths, row_offsets) = Self::rows_to_flat_paths(&rows);
        *self.flat_paths.borrow_mut() = flat_paths;
        *self.row_offsets.borrow_mut() = row_offsets;
        self.list_view.set_rows(rows);
        self.keybindings.set_row_count(self.list_view.row_count());
        let (row, col) = self.keybindings.selection();
        self.list_view.set_selection(row, col);
    }

    fn shuffled_items(&self, items: &[MediaItem]) -> Vec<MediaItem> {
        let mut shuffled = items.to_vec();
        let seed = self.shuffle_seed.get();
        shuffled.sort_by(|a, b| {
            let rank_a = Self::shuffle_rank_for_path(seed, &a.path);
            let rank_b = Self::shuffle_rank_for_path(seed, &b.path);
            rank_a.cmp(&rank_b).then_with(|| a.path.cmp(&b.path))
        });
        shuffled
    }

    fn new_shuffle_seed() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e3779b97f4a7c15)
    }

    fn shuffle_rank_for_path(seed: u64, path: &Path) -> u64 {
        let mut hasher = DefaultHasher::new();
        seed.hash(&mut hasher);
        path.hash(&mut hasher);
        hasher.finish()
    }

    fn update_shuffle_button(&self) {
        let label = if self.shuffle_mode.get() {
            "S/Shuffle: ON"
        } else {
            "S/Shuffle: OFF"
        };
        self.shuffle_button.set_label(label);
    }

    fn toggle_shuffle(&self) {
        let next = !self.shuffle_mode.get();
        self.shuffle_mode.set(next);
        if next {
            self.shuffle_seed.set(Self::new_shuffle_seed());
        }
        let base_items = self.base_items.borrow().clone();
        let items = if next {
            self.shuffled_items(&base_items)
        } else {
            base_items
        };
        *self.media_items.borrow_mut() = items.clone();
        let rows = self.build_rows_for_items(&items);
        reschedule_row_previews();
        self.apply_rows(rows);
        self.update_shuffle_button();
        self.set_status(&format!("> Shuffle: {}", if next { "ON" } else { "OFF" }));
    }

    /// Set up callback for when visible rows change
    fn setup_visible_range_callback(&self) {
        self.list_view
            .connect_visible_range_changed(move |first, last| {
                // This will be used to trigger thumbnail loading for visible rows
                tracing::debug!("Visible range: {} - {}", first, last);
            });
    }

    fn setup_layout_resize_observer(self: &Rc<Self>) {
        let weak_self = Rc::downgrade(self);
        let scrolled = self.list_view.widget().clone();
        scrolled.add_tick_callback(move |_widget, _clock| {
            if let Some(window) = weak_self.upgrade() {
                if window.stack.visible_child_name().as_deref() != Some("grid") {
                    return glib::ControlFlow::Continue;
                }
                window.refresh_sidebar_layout(false);
                let width = window.list_view.content_width().round() as i32;
                if width <= 0 {
                    return glib::ControlFlow::Continue;
                }
                let last = window.last_layout_width.get();
                if (width - last).abs() >= 1 {
                    window.last_layout_width.set(width);
                    window.schedule_grid_relayout_debounced(Duration::from_millis(80));
                }
            }
            glib::ControlFlow::Continue
        });
    }

    /// Load a directory and display its media files
    pub fn load_directory(&self, path: &Path) {
        self.set_active_source(ContentSource::Directory);
        self.set_status(&format!("> Scanning: {}", path.display()));
        reschedule_row_previews();
        self.set_current_path(Some(path.to_path_buf()));
        let viewport_width = self.list_view.content_width();
        let (list_alloc, scrolled_alloc, vscrollbar_width, vscrollbar_visible) =
            self.list_view.debug_allocations();
        let window_alloc = self.window.width();
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
        let generation = self.scan_generation.get().wrapping_add(1);
        self.scan_generation.set(generation);

        let path_buf = path.to_path_buf();
        let (tx, rx) = mpsc::channel::<(u64, DirectoryScanResult)>();
        let recursive = self.recursive_scan.get();
        std::thread::spawn(move || {
            let scanned = scan_directory(&path_buf, recursive);
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
        if !matches!(*self.active_source.borrow(), ContentSource::Directory) {
            return;
        }
        let current = self.current_path.borrow().clone();
        if current.as_deref() != Some(requested_path) {
            return;
        }

        self.apply_items(result.items);
        let items = self.media_items.borrow().clone();
        let folder_count = items.iter().filter(|i| i.is_folder()).count();
        let file_count = items.len().saturating_sub(folder_count);

        self.set_status(&format!(
            "> {} folders, {} files | {} rows | [hjkl/arrows] Navigate  [Enter] Open  [Backspace] Parent",
            folder_count,
            file_count,
            self.list_view.row_count()
        ));

        self.finalize_loaded_items();
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

    pub fn set_recursive_scan(&self, enabled: bool) {
        if self.recursive_scan.replace(enabled) != enabled {
            if matches!(*self.active_source.borrow(), ContentSource::Directory) {
                if let Some(current) = self.current_path() {
                    self.load_directory(&current);
                }
            }
        }
    }

    /// Handle item activation - either open viewer for media or navigate for folders
    fn handle_item_activation(&self, path: &Path) {
        if self.is_folder_path(path) {
            self.keybindings.set_view_mode(ViewMode::Grid);
            self.navigate_into_folder(path);
        } else {
            self.keybindings.set_view_mode(ViewMode::Viewer);
            self.open_viewer(path);
        }
    }

    fn path_at(&self, row: u32, col: u32) -> Option<PathBuf> {
        use super::list_view::RowModelObject;
        if let Some(obj) = self
            .list_view
            .model()
            .item(row)
            .and_downcast::<RowModelObject>()
        {
            if let Some(row_model) = obj.row_model() {
                if let Some(item) = row_model.items.get(col as usize) {
                    return Some(item.media_path.clone());
                }
            }
        }
        None
    }

    fn selection_for_path(&self, path: &Path) -> Option<(u32, u32)> {
        use super::list_view::RowModelObject;
        let model = self.list_view.model();
        for row in 0..model.n_items() {
            let Some(obj) = model.item(row).and_downcast::<RowModelObject>() else {
                continue;
            };
            let Some(row_model) = obj.row_model() else {
                continue;
            };
            for (col, item) in row_model.items.iter().enumerate() {
                if item.media_path == path {
                    return Some((row, col as u32));
                }
            }
        }
        None
    }

    fn adjacent_media_path(&self, path: &Path) -> Option<PathBuf> {
        let flat_paths = self.flat_paths.borrow();
        let idx = flat_paths.iter().position(|p| p == path)?;
        let folders = self.folder_paths.borrow();

        for candidate in flat_paths.iter().skip(idx + 1) {
            if !folders.contains(candidate) {
                return Some(candidate.clone());
            }
        }

        for candidate in flat_paths.iter().take(idx).rev() {
            if !folders.contains(candidate) {
                return Some(candidate.clone());
            }
        }

        None
    }

    fn favorite_state_for_path(&self, path: &Path) -> Option<bool> {
        let store = self.media_store.borrow();
        let store = store.as_ref()?;
        match store.is_favorite(path) {
            Ok(state) => Some(state),
            Err(err) => {
                tracing::warn!(error = ?err, "Failed to read favorite state");
                None
            }
        }
    }

    fn toggle_favorite_selected(&self) {
        let (row, col) = self.keybindings.selection();
        let Some(path) = self.path_at(row, col) else {
            return;
        };
        let _ = self.toggle_favorite_for_path(&path);
    }

    fn toggle_favorite_for_path(&self, path: &Path) -> Option<bool> {
        if self.is_folder_path(&path) {
            self.set_status("> Favorites apply to files only");
            return None;
        }
        let store = self.media_store.borrow();
        let Some(store) = store.as_ref() else {
            self.set_status("> Favorites unavailable (database error)");
            return None;
        };
        match store.toggle_favorite(&path) {
            Ok(true) => {
                self.set_status(&format!(
                    "> Favorited: {}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("[item]")
                ));
                if matches!(*self.active_source.borrow(), ContentSource::Favorites) {
                    self.reload_active_source();
                }
                if self.viewer.current_path().as_deref() == Some(path) {
                    self.viewer.set_favorite_state(true);
                }
                Some(true)
            }
            Ok(false) => {
                self.set_status(&format!(
                    "> Unfavorited: {}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("[item]")
                ));
                if matches!(*self.active_source.borrow(), ContentSource::Favorites) {
                    self.reload_active_source();
                }
                if self.viewer.current_path().as_deref() == Some(path) {
                    self.viewer.set_favorite_state(false);
                }
                Some(false)
            }
            Err(err) => {
                tracing::warn!(error = ?err, "Failed to toggle favorite");
                self.set_status("> Failed to update favorite");
                None
            }
        }
    }

    fn delete_selected(&self) {
        let (row, col) = self.keybindings.selection();
        let Some(path) = self.path_at(row, col) else {
            return;
        };
        if self.is_folder_path(&path) {
            self.set_status("> Delete applies to files only");
            return;
        }
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("[item]");
        let was_viewer_open = self.stack.visible_child_name().as_deref() == Some("viewer");
        let next_media = if was_viewer_open {
            self.adjacent_media_path(&path)
        } else {
            None
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {
                self.set_status(&format!("> Deleted: {}", filename));
                *self.pending_viewer_target.borrow_mut() = if was_viewer_open {
                    next_media.clone()
                } else {
                    None
                };
                if was_viewer_open && next_media.is_none() {
                    self.viewer.hide();
                    self.keybindings.set_view_mode(ViewMode::Grid);
                    self.stack.set_visible_child_name("grid");
                }
                self.reload_active_source();
            }
            Err(err) => {
                tracing::warn!(error = ?err, "Failed to delete file");
                self.set_status(&format!("> Failed to delete: {}", filename));
            }
        }
    }

    fn add_path_to_album(&self, album_id: i64, path: &Path) {
        let store = self.media_store.borrow();
        let Some(store) = store.as_ref() else {
            self.set_status("> Albums unavailable (database error)");
            return;
        };
        match store.add_to_album(album_id, path) {
            Ok(true) => {
                self.set_status(&format!(
                    "> Added to album: {}",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("[item]")
                ));
                if matches!(
                    *self.active_source.borrow(),
                    ContentSource::Album { id, .. } if id == album_id
                ) {
                    self.reload_active_source();
                }
            }
            Ok(false) => self.set_status("> Already in album"),
            Err(err) => {
                tracing::warn!(error = ?err, "Failed to add to album");
                self.set_status("> Failed to add to album");
            }
        }
    }

    fn create_album_and_add(&self, name: &str, path: &Path) {
        let album_id = {
            let store = self.media_store.borrow();
            let Some(store) = store.as_ref() else {
                self.set_status("> Albums unavailable (database error)");
                return;
            };
            match store.create_album(name) {
                Ok(album_id) => album_id,
                Err(err) => {
                    tracing::warn!(error = ?err, "Failed to create album");
                    self.set_status("> Failed to create album");
                    return;
                }
            }
        };
        self.add_path_to_album(album_id, path);
        self.rebuild_sidebar();
    }

    fn prompt_new_album(&self, path: PathBuf) {
        let shell = self.build_dialog_shell("New Album", 360);
        let name_label = Label::new(Some("Album name"));
        name_label.set_halign(Align::Start);
        shell.content.append(&name_label);

        let name_entry = Entry::new();
        name_entry.set_placeholder_text(Some("e.g. Favorites 2026"));
        shell.content.append(&name_entry);

        let actions = GtkBox::new(Orientation::Horizontal, 8);
        let create_button = Button::with_label("Create");
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        actions.append(&spacer);
        actions.append(&create_button);
        shell.content.append(&actions);

        let dialog = shell.dialog.clone();
        let dialog_for_create = dialog.clone();
        let window_weak = self.self_weak.borrow().clone();
        create_button.connect_clicked(move |_| {
            let name = name_entry.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            if let Some(window) = window_weak.upgrade() {
                window.create_album_and_add(&name, &path);
            }
            dialog_for_create.close();
        });

        let dialog_for_close = dialog.clone();
        shell.close_button.connect_clicked(move |_| {
            dialog_for_close.close();
        });

        shell.dialog.present();
    }

    fn show_album_menu(&self, path: PathBuf, anchor: &gtk4::Widget, rect: Rectangle) {
        if self.is_folder_path(&path) {
            return;
        }
        let store = self.media_store.borrow();
        let Some(store) = store.as_ref() else {
            self.set_status("> Albums unavailable (database error)");
            return;
        };
        let albums = match store.list_albums() {
            Ok(albums) => albums,
            Err(err) => {
                tracing::warn!(error = ?err, "Failed to list albums");
                self.set_status("> Failed to load albums");
                return;
            }
        };
        let is_favorite = match store.is_favorite(&path) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(error = ?err, "Failed to read favorite status");
                self.set_status("> Failed to load favourite status");
                false
            }
        };
        let album_membership: HashSet<i64> = match store.album_ids_for_path(&path) {
            Ok(ids) => ids.into_iter().collect(),
            Err(err) => {
                tracing::warn!(error = ?err, "Failed to read album membership");
                self.set_status("> Failed to load album membership");
                HashSet::new()
            }
        };

        let popover = gtk4::Popover::new();
        popover.add_css_class("context-menu-popover");
        popover.set_has_arrow(true);
        popover.set_position(gtk4::PositionType::Bottom);
        let anchor_point = graphene::Point::new(rect.x() as f32, rect.y() as f32);
        let (px, py) = anchor
            .compute_point(&self.window, &anchor_point)
            .map(|p| (p.x(), p.y()))
            .unwrap_or((rect.x() as f32, rect.y() as f32));
        let pointing = Rectangle::new(
            px.round() as i32,
            py.round() as i32,
            rect.width(),
            rect.height(),
        );
        popover.set_pointing_to(Some(&pointing));
        popover.set_autohide(true);
        popover.set_parent(&self.window);

        let content = GtkBox::new(Orientation::Vertical, 6);
        content.add_css_class("context-menu");
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(8);
        content.set_margin_end(8);

        let header = Label::new(Some("[ FILE MENU ]"));
        header.set_halign(Align::Start);
        header.add_css_class("context-menu-title");
        content.append(&header);

        let sep = gtk4::Separator::new(Orientation::Horizontal);
        sep.add_css_class("context-menu-separator");
        content.append(&sep);

        let favourite_label = if is_favorite {
            "[x] Favourite"
        } else {
            "[ ] Favourite"
        };
        let favourite_button = Button::with_label(favourite_label);
        favourite_button.add_css_class("context-menu-item");
        if is_favorite {
            favourite_button.add_css_class("context-menu-item-checked");
        }
        favourite_button.set_halign(Align::Fill);
        favourite_button.set_hexpand(true);
        let window_weak = self.self_weak.borrow().clone();
        let path_for_favourite = path.clone();
        let popover_for_favourite = popover.clone();
        favourite_button.connect_clicked(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.toggle_favorite_for_path(&path_for_favourite);
            }
            popover_for_favourite.popdown();
        });
        content.append(&favourite_button);

        let sep = gtk4::Separator::new(Orientation::Horizontal);
        sep.add_css_class("context-menu-separator");
        content.append(&sep);

        if albums.is_empty() {
            let empty = Label::new(Some("No albums yet"));
            empty.add_css_class("context-menu-empty");
            content.append(&empty);
        }

        let window_weak = self.self_weak.borrow().clone();
        for (album_id, name) in albums {
            let in_album = album_membership.contains(&album_id);
            let label = if in_album {
                format!("[x] {}", name)
            } else {
                format!("[ ] {}", name)
            };
            let button = Button::with_label(&label);
            button.add_css_class("context-menu-item");
            if in_album {
                button.add_css_class("context-menu-item-checked");
            }
            button.set_halign(Align::Fill);
            button.set_hexpand(true);
            let window_weak = window_weak.clone();
            let path = path.clone();
            let popover = popover.clone();
            button.connect_clicked(move |_| {
                if let Some(window) = window_weak.upgrade() {
                    window.add_path_to_album(album_id, &path);
                }
                popover.popdown();
            });
            content.append(&button);
        }

        let sep = gtk4::Separator::new(Orientation::Horizontal);
        sep.add_css_class("context-menu-separator");
        content.append(&sep);

        let new_album_btn = Button::with_label("+ New album...");
        new_album_btn.add_css_class("context-menu-item");
        new_album_btn.add_css_class("context-menu-create");
        new_album_btn.set_halign(Align::Fill);
        new_album_btn.set_hexpand(true);
        let window_weak = self.self_weak.borrow().clone();
        let path_for_new = path.clone();
        let popover_for_new = popover.clone();
        new_album_btn.connect_clicked(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.prompt_new_album(path_for_new.clone());
            }
            popover_for_new.popdown();
        });
        content.append(&new_album_btn);

        popover.set_child(Some(&content));
        popover.popup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::RowItem;
    use std::path::PathBuf;

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

    #[test]
    fn effective_layout_width_rejects_invalid_numbers() {
        let width = MainWindow::effective_layout_width(f32::NAN, 0, 0, f32::INFINITY);
        assert!((width - 1.0).abs() < 0.01);
    }

    #[test]
    fn clamp_rows_to_width_scales_oversized_rows() {
        let rows = vec![RowModel {
            row_index: 0,
            height_px: 220.0,
            items: vec![
                RowItem {
                    media_path: PathBuf::from("a.jpg"),
                    display_w: 900.0,
                    display_h: 220.0,
                    offset_top: 0.0,
                    is_folder: false,
                },
                RowItem {
                    media_path: PathBuf::from("b.jpg"),
                    display_w: 700.0,
                    display_h: 220.0,
                    offset_top: 0.0,
                    is_folder: false,
                },
            ],
        }];

        let clamped = MainWindow::clamp_rows_to_width(rows, 1200.0);
        let total_width: f32 = clamped[0].items.iter().map(|item| item.display_w).sum();
        assert!(total_width <= 1200.0 + 0.01);
    }

    #[test]
    fn clamp_rows_to_width_sanitizes_invalid_item_dimensions() {
        let rows = vec![RowModel {
            row_index: 0,
            height_px: 220.0,
            items: vec![
                RowItem {
                    media_path: PathBuf::from("a.jpg"),
                    display_w: f32::NAN,
                    display_h: f32::INFINITY,
                    offset_top: -3.0,
                    is_folder: false,
                },
                RowItem {
                    media_path: PathBuf::from("b.jpg"),
                    display_w: -1.0,
                    display_h: 100.0,
                    offset_top: f32::NAN,
                    is_folder: false,
                },
            ],
        }];

        let clamped = MainWindow::clamp_rows_to_width(rows, 1200.0);
        assert!(clamped[0].height_px.is_finite());
        for item in &clamped[0].items {
            assert!(item.display_w.is_finite() && item.display_w >= 1.0);
            assert!(item.display_h.is_finite() && item.display_h >= 1.0);
            assert!(item.offset_top.is_finite() && item.offset_top >= 0.0);
        }
    }
}
