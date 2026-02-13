// Row widget for displaying a horizontal row of media items
// Uses a reusable factory pattern with placeholder textures

use gdk4::Texture;
use glib::Object;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{
    gdk, glib, Align, Box as GtkBox, ContentFit, GestureClick, Label, Orientation, Overlay,
    Picture, Widget,
};
use image::imageops::FilterType;
use image::GenericImageView;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use crate::models::RowModel;

const ROW_PREVIEW_SIZE: u32 = 512;
const ROW_LOADER_MAX_THREADS: usize = 8;
const ROW_LOADER_MIN_THREADS: usize = 2;
const ROW_LOADER_QUEUE: usize = 512;
const ROW_CACHE_ENTRIES: usize = 1024;
const ROW_RESULTS_PER_TICK: usize = 12;
const VIDEO_PREVIEW_START_SECS: [f64; 2] = [1.0, 0.0];

fn row_loader_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| {
            n.get()
                .saturating_sub(2)
                .clamp(ROW_LOADER_MIN_THREADS, ROW_LOADER_MAX_THREADS)
        })
        .unwrap_or(4)
}

// Placeholder texture - generated once and reused
fn placeholder_texture() -> &'static Texture {
    static PLACEHOLDER: OnceLock<Texture> = OnceLock::new();
    PLACEHOLDER.get_or_init(|| {
        // Create a simple dark gray placeholder texture (64x64)
        let width = 64;
        let height = 64;
        let mut pixels = vec![0u8; width * height * 4];

        // Fill with dark gray (#1a1a1a) RGBA
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[0] = 0x1a; // R
            chunk[1] = 0x1a; // G
            chunk[2] = 0x1a; // B
            chunk[3] = 0xff; // A
        }

        let bytes = glib::Bytes::from_owned(pixels);
        Texture::from_bytes(&bytes).unwrap_or_else(|_| {
            // Fallback: create texture from memory
            gdk::MemoryTexture::new(
                width as i32,
                height as i32,
                gdk::MemoryFormat::R8g8b8a8,
                &bytes,
                width * 4,
            )
            .upcast()
        })
    })
}

// Folder icon texture - a simple folder shape
fn folder_texture() -> &'static Texture {
    static FOLDER: OnceLock<Texture> = OnceLock::new();
    FOLDER.get_or_init(|| {
        let width: usize = 128;
        let height: usize = 128;
        let mut pixels = vec![0u8; width * height * 4];

        // Background: dark gray (#121212)
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[0] = 0x12; // R
            chunk[1] = 0x12; // G
            chunk[2] = 0x12; // B
            chunk[3] = 0xff; // A
        }

        // Draw a simple folder icon in terminal green (#00ff88)
        // Folder shape: tab at top-left, rectangular body
        let folder_color = [0x00u8, 0xff, 0x88, 0xff];
        let border_color = [0x33u8, 0x33, 0x33, 0xff];

        // Folder dimensions (centered in 128x128)
        let left = 20;
        let right = 108;
        let top = 35;
        let bottom = 95;
        let tab_width = 35;
        let tab_height = 12;

        // Draw folder body outline
        for y in top..=bottom {
            for x in left..=right {
                let is_border = x == left || x == right || y == top || y == bottom;
                let is_tab_area = y < top + tab_height && x < left + tab_width;
                let is_tab_top = y == top - tab_height + 1 && x >= left && x < left + tab_width;
                let is_tab_side = x == left + tab_width - 1 && y >= top - tab_height + 1 && y < top;

                if is_border || is_tab_top || is_tab_side {
                    let idx = (y * width + x) * 4;
                    if idx + 3 < pixels.len() {
                        if is_tab_area && !is_border {
                            // Inside tab - use folder color
                            pixels[idx..idx + 4].copy_from_slice(&folder_color);
                        } else {
                            // Border
                            pixels[idx..idx + 4].copy_from_slice(&border_color);
                        }
                    }
                }
            }
        }

        // Draw tab (above main body)
        let tab_top = top - tab_height + 1;
        for y in tab_top..top {
            for x in left..left + tab_width {
                let idx = (y * width + x) * 4;
                if idx + 3 < pixels.len() {
                    let is_border = x == left || y == tab_top || x == left + tab_width - 1;
                    if is_border {
                        pixels[idx..idx + 4].copy_from_slice(&border_color);
                    }
                }
            }
        }

        // Fill folder body interior with slightly lighter shade
        let fill_color = [0x1a, 0x1a, 0x1a, 0xff];
        for y in (top + 1)..bottom {
            for x in (left + 1)..right {
                let idx = (y * width + x) * 4;
                if idx + 3 < pixels.len() {
                    pixels[idx..idx + 4].copy_from_slice(&fill_color);
                }
            }
        }

        let bytes = glib::Bytes::from_owned(pixels);
        gdk::MemoryTexture::new(
            width as i32,
            height as i32,
            gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            width * 4,
        )
        .upcast()
    })
}

#[derive(Debug)]
struct RowDecodeRequest {
    path: PathBuf,
    generation: u64,
}

#[derive(Debug)]
struct RowDecodeResult {
    path: PathBuf,
    rgba: Option<Vec<u8>>,
    width: u32,
    height: u32,
}

#[derive(Clone)]
struct RowWaiter {
    widget: glib::WeakRef<RowWidget>,
    index: usize,
    token: u64,
}

struct RowLoaderState {
    pending_paths: HashSet<PathBuf>,
    waiters: HashMap<PathBuf, Vec<RowWaiter>>,
    cache: lru::LruCache<PathBuf, Texture>,
}

struct RowImageLoader {
    request_tx: flume::Sender<RowDecodeRequest>,
    request_rx: flume::Receiver<RowDecodeRequest>,
    result_rx: flume::Receiver<RowDecodeResult>,
    generation: std::sync::Arc<AtomicU64>,
    state: RefCell<RowLoaderState>,
}

static NEXT_LOAD_TOKEN: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static ROW_IMAGE_LOADER: Rc<RowImageLoader> = RowImageLoader::new();
}

impl RowImageLoader {
    fn new() -> Rc<Self> {
        let (request_tx, request_rx) = flume::bounded::<RowDecodeRequest>(ROW_LOADER_QUEUE);
        let (result_tx, result_rx) = flume::unbounded::<RowDecodeResult>();
        let generation = std::sync::Arc::new(AtomicU64::new(1));

        for _ in 0..row_loader_threads() {
            let rx = request_rx.clone();
            let tx = result_tx.clone();
            let generation = generation.clone();
            std::thread::spawn(move || {
                while let Ok(req) = rx.recv() {
                    if req.generation != generation.load(Ordering::Acquire) {
                        continue;
                    }
                    let decoded = decode_row_preview(&req.path);
                    let (rgba, width, height) = match decoded {
                        Some((data, w, h)) => (Some(data), w, h),
                        None => (None, 0, 0),
                    };
                    let _ = tx.send(RowDecodeResult {
                        path: req.path,
                        rgba,
                        width,
                        height,
                    });
                }
            });
        }

        let loader = Rc::new(Self {
            request_tx,
            request_rx,
            result_rx,
            generation,
            state: RefCell::new(RowLoaderState {
                pending_paths: HashSet::new(),
                waiters: HashMap::new(),
                cache: lru::LruCache::new(NonZeroUsize::new(ROW_CACHE_ENTRIES).unwrap()),
            }),
        });

        let loader_weak = Rc::downgrade(&loader);
        glib::timeout_add_local(Duration::from_millis(16), move || {
            if let Some(loader) = loader_weak.upgrade() {
                loader.process_results();
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });

        loader
    }

    fn request(&self, row_widget: &RowWidget, index: usize, path: &Path, token: u64) {
        let mut state = self.state.borrow_mut();

        if let Some(texture) = state.cache.get(path).cloned() {
            let widget_weak = row_widget.downgrade();
            let path = path.to_path_buf();
            glib::idle_add_local_once(move || {
                if let Some(row_widget) = widget_weak.upgrade() {
                    row_widget.apply_async_texture(index, token, &path, Some(&texture));
                }
            });
            return;
        }

        state
            .waiters
            .entry(path.to_path_buf())
            .or_default()
            .push(RowWaiter {
                widget: row_widget.downgrade(),
                index,
                token,
            });

        if state.pending_paths.insert(path.to_path_buf()) {
            let generation = self.generation.load(Ordering::Acquire);
            if self
                .request_tx
                .try_send(RowDecodeRequest {
                    path: path.to_path_buf(),
                    generation,
                })
                .is_err()
            {
                state.pending_paths.remove(path);
                state.waiters.remove(path);
            }
        }
    }

    fn cached_texture(&self, path: &Path) -> Option<Texture> {
        self.state.borrow_mut().cache.get(path).cloned()
    }

    fn reschedule(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self.state.borrow_mut();
        state.pending_paths.clear();
        state.waiters.clear();
        drop(state);
        while self.request_rx.try_recv().is_ok() {}
    }

    fn process_results(&self) {
        for _ in 0..ROW_RESULTS_PER_TICK {
            let Ok(result) = self.result_rx.try_recv() else {
                break;
            };
            let texture = result
                .rgba
                .and_then(|rgba| create_texture_from_rgba(rgba, result.width, result.height));

            let waiters = {
                let mut state = self.state.borrow_mut();
                state.pending_paths.remove(&result.path);
                if let Some(ref texture) = texture {
                    state.cache.put(result.path.clone(), texture.clone());
                }
                state.waiters.remove(&result.path).unwrap_or_default()
            };

            for waiter in waiters {
                if let Some(row_widget) = waiter.widget.upgrade() {
                    row_widget.apply_async_texture(
                        waiter.index,
                        waiter.token,
                        &result.path,
                        texture.as_ref(),
                    );
                }
            }
        }
    }
}

pub fn reschedule_row_previews() {
    ROW_IMAGE_LOADER.with(|loader| loader.reschedule());
}

pub fn cached_row_preview_texture(path: &Path) -> Option<Texture> {
    ROW_IMAGE_LOADER.with(|loader| loader.cached_texture(path))
}

fn decode_row_preview(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let img = if is_video_path(path) {
        decode_video_preview(path)?
    } else {
        crate::image_loader::open_image(path).ok()?
    };
    let (src_w, src_h) = img.dimensions();
    let resized = if src_w <= ROW_PREVIEW_SIZE && src_h <= ROW_PREVIEW_SIZE {
        img
    } else {
        // Fast resize path tuned for smooth grid scrolling.
        let scale_w = ROW_PREVIEW_SIZE as f32 / src_w as f32;
        let scale_h = ROW_PREVIEW_SIZE as f32 / src_h as f32;
        let scale = scale_w.min(scale_h);
        let new_w = ((src_w as f32 * scale).round() as u32).max(1);
        let new_h = ((src_h as f32 * scale).round() as u32).max(1);
        img.resize_exact(new_w, new_h, FilterType::Triangle)
    };
    let (width, height) = resized.dimensions();
    let rgba = resized.to_rgba8().into_raw();
    Some((rgba, width.max(1), height.max(1)))
}

fn is_video_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "mp4" | "webm" | "mkv" | "avi" | "mov"
            )
        })
        .unwrap_or(false)
}

fn decode_video_preview(path: &Path) -> Option<image::DynamicImage> {
    VIDEO_PREVIEW_START_SECS
        .iter()
        .copied()
        .find_map(|start_seconds| mpv_extract_frame(path, start_seconds))
}

struct VideoPreviewDir {
    path: PathBuf,
}

impl VideoPreviewDir {
    fn new() -> Option<Self> {
        static NEXT_PREVIEW_DIR_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_PREVIEW_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("idxd-mpv-thumb-{}-{}", std::process::id(), id));
        fs::create_dir_all(&path).ok()?;
        Some(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for VideoPreviewDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn mpv_extract_frame(path: &Path, start_seconds: f64) -> Option<image::DynamicImage> {
    let out_dir = VideoPreviewDir::new()?;
    let out_dir_str = out_dir.path().to_str()?;
    let start = format!("{start_seconds:.3}");

    run_mpv_command(path, out_dir_str, &start)?;
    let image_path = find_generated_preview_file(out_dir.path())?;
    image::open(image_path).ok()
}

fn find_generated_preview_file(dir: &Path) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_file())
        .collect();

    files.sort();
    files.into_iter().next_back()
}

fn run_mpv_command(path: &Path, out_dir: &str, start_seconds: &str) -> Option<()> {
    if run_mpv_command_impl(Command::new("mpv"), path, out_dir, start_seconds, "mpv") {
        return Some(());
    }

    // Flatpak fallback: use host mpv if app runtime doesn't provide it.
    if std::env::var_os("FLATPAK_ID").is_some() {
        let mut cmd = Command::new("flatpak-spawn");
        cmd.arg("--host").arg("mpv");
        if run_mpv_command_impl(
            cmd,
            path,
            out_dir,
            start_seconds,
            "flatpak-spawn --host mpv",
        ) {
            return Some(());
        }
    }

    if run_gst_thumbnail_command(path, out_dir) {
        return Some(());
    }

    tracing::debug!(
        path = %path.display(),
        "Video thumbnail extraction failed: mpv/gstreamer unavailable or failed"
    );
    None
}

fn run_mpv_command_impl(
    mut cmd: Command,
    path: &Path,
    out_dir: &str,
    start_seconds: &str,
    command_label: &str,
) -> bool {
    let start_arg = format!("--start={start_seconds}");
    let outdir_arg = format!("--vo-image-outdir={out_dir}");

    let result = cmd
        .arg("--no-config")
        .arg("--no-terminal")
        .arg("--msg-level=all=error")
        .arg("--ao=null")
        .arg("--vo=image")
        .arg("--vo-image-format=png")
        .arg(outdir_arg)
        .arg("--frames=1")
        .arg(start_arg)
        .arg("--")
        .arg(path)
        .output();

    match result {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::debug!(
                command = command_label,
                path = %path.display(),
                status = %output.status,
                stderr = stderr.trim(),
                "mpv thumbnail command failed"
            );
            false
        }
        Err(err) => {
            tracing::debug!(
                command = command_label,
                path = %path.display(),
                error = ?err,
                "Failed to spawn mpv thumbnail command"
            );
            false
        }
    }
}

fn run_gst_thumbnail_command(path: &Path, out_dir: &str) -> bool {
    let output_path = Path::new(out_dir).join("thumb-gst.png");
    if run_gst_thumbnail_command_impl(
        Command::new("gst-launch-1.0"),
        path,
        &output_path,
        "gst-launch-1.0",
    ) {
        return true;
    }

    if std::env::var_os("FLATPAK_ID").is_some() {
        let mut cmd = Command::new("flatpak-spawn");
        cmd.arg("--host").arg("gst-launch-1.0");
        if run_gst_thumbnail_command_impl(
            cmd,
            path,
            &output_path,
            "flatpak-spawn --host gst-launch-1.0",
        ) {
            return true;
        }
    }

    false
}

fn run_gst_thumbnail_command_impl(
    mut cmd: Command,
    path: &Path,
    output_path: &Path,
    command_label: &str,
) -> bool {
    let location_arg = format!("location={}", path.to_string_lossy());
    let output_arg = format!("location={}", output_path.to_string_lossy());

    let result = cmd
        .arg("-q")
        .arg("filesrc")
        .arg(location_arg)
        .arg("!")
        .arg("decodebin")
        .arg("!")
        .arg("videoconvert")
        .arg("!")
        .arg("pngenc")
        .arg("snapshot=true")
        .arg("!")
        .arg("filesink")
        .arg(output_arg)
        .output();

    match result {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::debug!(
                command = command_label,
                path = %path.display(),
                status = %output.status,
                stderr = stderr.trim(),
                "gstreamer thumbnail command failed"
            );
            false
        }
        Err(err) => {
            tracing::debug!(
                command = command_label,
                path = %path.display(),
                error = ?err,
                "Failed to spawn gstreamer thumbnail command"
            );
            false
        }
    }
}

fn create_texture_from_rgba(rgba: Vec<u8>, width: u32, height: u32) -> Option<Texture> {
    if width == 0 || height == 0 {
        return None;
    }
    let expected = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    if rgba.len() < expected {
        return None;
    }
    let bytes = glib::Bytes::from_owned(rgba);
    let texture = gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        (width * 4) as usize,
    );
    Some(texture.upcast())
}

// GObject subclass for RowWidget
mod imp {
    use super::*;

    /// Wrapper for a single item slot (either a plain Picture or an Overlay with folder name)
    pub struct ItemSlot {
        pub widget: gtk4::Widget,
        pub picture: Picture,
        pub overlay: Option<Overlay>,
        pub label: Option<Label>,
        pub video_badge: Option<Label>,
    }

    #[derive(Default)]
    pub struct RowWidgetInner {
        pub container: RefCell<Option<GtkBox>>,
        pub slots: RefCell<Vec<ItemSlot>>,
        pub load_tokens: RefCell<Vec<u64>>,
        pub item_paths: RefCell<Vec<PathBuf>>,
        pub item_is_folder: RefCell<Vec<bool>>,
        pub row_index: Cell<u32>,
        pub on_item_activated: RefCell<Option<Rc<dyn Fn(u32, u32, PathBuf)>>>,
        pub on_item_context_menu:
            RefCell<Option<Rc<dyn Fn(u32, u32, PathBuf, Widget, gdk::Rectangle)>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RowWidgetInner {
        const NAME: &'static str = "IdxdRowWidget";
        type Type = super::RowWidget;
        type ParentType = GtkBox;
    }

    impl ObjectImpl for RowWidgetInner {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();
            obj.set_orientation(Orientation::Horizontal);
            obj.set_spacing(0);
            obj.set_homogeneous(false);
            obj.set_hexpand(true);
            obj.set_halign(Align::Fill);
            obj.set_valign(Align::Start);
            obj.add_css_class("media-row");
        }
    }

    impl WidgetImpl for RowWidgetInner {}
    impl BoxImpl for RowWidgetInner {}
}

glib::wrapper! {
    pub struct RowWidget(ObjectSubclass<imp::RowWidgetInner>)
        @extends GtkBox, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Orientable;
}

impl RowWidget {
    pub fn new() -> Self {
        Object::builder().build()
    }

    /// Bind a RowModel to this widget, creating/updating picture widgets as needed
    pub fn bind(&self, row_model: &RowModel) {
        let imp = self.imp();
        let items = &row_model.items;

        let mut slots = imp.slots.borrow_mut();
        let mut load_tokens = imp.load_tokens.borrow_mut();
        let mut paths = imp.item_paths.borrow_mut();
        let mut is_folder_vec = imp.item_is_folder.borrow_mut();
        imp.row_index.set(row_model.row_index);

        // Ensure we have the right number of item slots
        while slots.len() < items.len() {
            let slot = self.create_item_slot(slots.len() as u32);
            self.append(&slot.widget);
            slots.push(slot);
            load_tokens.push(0);
        }

        // Hide extra slots if we have too many
        for (i, slot) in slots.iter().enumerate() {
            if i < items.len() {
                slot.widget.set_visible(true);
            } else {
                slot.widget.set_visible(false);
                load_tokens[i] = 0;
            }
        }

        // Update paths and folder flags
        paths.clear();
        is_folder_vec.clear();
        paths.extend(items.iter().map(|item| item.media_path.clone()));
        is_folder_vec.extend(items.iter().map(|item| item.is_folder));

        // Update item dimensions and content
        for (i, item) in items.iter().enumerate() {
            let slot = &slots[i];
            let width = (item.display_w.floor() as i32).max(1);
            let height = (item.display_h.round() as i32).max(1);

            slot.widget.set_size_request(width, height);
            slot.picture.set_size_request(width, height);

            if item.is_folder {
                // Display folder with icon and name
                slot.picture.set_paintable(Some(folder_texture()));
                load_tokens[i] = 0;

                // Update or show the label with folder name
                if let Some(ref label) = slot.label {
                    let folder_name = item
                        .media_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("[folder]");
                    label.set_text(folder_name);
                    label.set_visible(true);
                }
                if let Some(ref badge) = slot.video_badge {
                    badge.set_visible(false);
                }
            } else {
                // Hide label for non-folder items
                if let Some(ref label) = slot.label {
                    label.set_visible(false);
                }
                if let Some(ref badge) = slot.video_badge {
                    badge.set_visible(is_video_path(&item.media_path));
                }

                slot.picture.set_paintable(Some(placeholder_texture()));
                let token = NEXT_LOAD_TOKEN.fetch_add(1, Ordering::Relaxed);
                load_tokens[i] = token;
                ROW_IMAGE_LOADER.with(|loader| {
                    loader.request(self, i, &item.media_path, token);
                });
            }
        }
    }

    /// Unbind the current row model, preparing for reuse
    pub fn unbind(&self) {
        let imp = self.imp();
        let slots = imp.slots.borrow();

        // Reset all slots to placeholder
        for slot in slots.iter() {
            slot.picture.set_paintable(Some(placeholder_texture()));
            slot.widget.set_visible(false);
            slot.widget.remove_css_class("selected");
            if let Some(ref label) = slot.label {
                label.set_visible(false);
            }
            if let Some(ref badge) = slot.video_badge {
                badge.set_visible(false);
            }
        }
        for token in imp.load_tokens.borrow_mut().iter_mut() {
            *token = 0;
        }

        imp.item_paths.borrow_mut().clear();
        imp.item_is_folder.borrow_mut().clear();
    }

    /// Update the texture for a specific item in this row
    pub fn set_texture(&self, index: usize, texture: &Texture) {
        let slots = self.imp().slots.borrow();
        if let Some(slot) = slots.get(index) {
            slot.picture.set_paintable(Some(texture));
        }
    }

    fn apply_async_texture(
        &self,
        index: usize,
        token: u64,
        expected_path: &Path,
        texture: Option<&Texture>,
    ) {
        if texture.is_none() {
            return;
        }
        let imp = self.imp();
        let tokens = imp.load_tokens.borrow();
        if tokens.get(index).copied() != Some(token) {
            return;
        }
        drop(tokens);
        if self.get_item_path(index).as_deref() != Some(expected_path) {
            return;
        }
        let slots = imp.slots.borrow();
        if let (Some(slot), Some(texture)) = (slots.get(index), texture) {
            slot.picture.set_paintable(Some(texture));
        }
    }

    /// Get the path for a specific item in this row
    pub fn get_item_path(&self, index: usize) -> Option<PathBuf> {
        self.imp().item_paths.borrow().get(index).cloned()
    }

    /// Get all item paths in this row
    pub fn get_item_paths(&self) -> Vec<PathBuf> {
        self.imp().item_paths.borrow().clone()
    }

    /// Get the number of items currently displayed
    pub fn item_count(&self) -> usize {
        self.imp()
            .slots
            .borrow()
            .iter()
            .filter(|s| s.widget.is_visible())
            .count()
    }

    /// Check if the item at the given index is a folder
    pub fn is_folder(&self, index: usize) -> bool {
        self.imp()
            .item_is_folder
            .borrow()
            .get(index)
            .copied()
            .unwrap_or(false)
    }

    pub fn update_selection(&self, selected_row: u32, selected_col: u32) {
        let imp = self.imp();
        let row = imp.row_index.get();
        let slots = imp.slots.borrow();
        for (i, slot) in slots.iter().enumerate() {
            if !slot.widget.is_visible() {
                slot.widget.remove_css_class("selected");
                continue;
            }
            if row == selected_row && i == selected_col as usize {
                slot.widget.add_css_class("selected");
            } else {
                slot.widget.remove_css_class("selected");
            }
        }
    }

    pub fn connect_item_activated<F>(&self, callback: F)
    where
        F: Fn(u32, u32, PathBuf) + 'static,
    {
        *self.imp().on_item_activated.borrow_mut() = Some(Rc::new(callback));
    }

    pub fn connect_item_context_menu<F>(&self, callback: F)
    where
        F: Fn(u32, u32, PathBuf, Widget, gdk::Rectangle) + 'static,
    {
        *self.imp().on_item_context_menu.borrow_mut() = Some(Rc::new(callback));
    }

    fn create_item_slot(&self, index: u32) -> imp::ItemSlot {
        let picture = Picture::new();
        // Allow the widget to shrink to the allocated size; otherwise large
        // images only render a clipped fragment.
        picture.set_can_shrink(true);
        picture.set_content_fit(ContentFit::Contain);
        picture.set_halign(Align::Center);
        picture.set_valign(Align::Center);
        picture.add_css_class("media-item");

        // Create an overlay for folder name display
        let overlay = Overlay::new();
        overlay.set_child(Some(&picture));
        overlay.add_css_class("media-item");

        // Create label for folder name (hidden by default)
        let label = Label::new(None);
        label.set_halign(Align::Center);
        label.set_valign(Align::End);
        label.set_margin_bottom(8);
        label.add_css_class("folder-name");
        label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        label.set_max_width_chars(15);
        label.set_visible(false);
        overlay.add_overlay(&label);

        let video_badge = Label::new(Some("[V]"));
        video_badge.set_halign(Align::Start);
        video_badge.set_valign(Align::Start);
        video_badge.set_margin_start(6);
        video_badge.set_margin_top(4);
        video_badge.add_css_class("video-badge");
        video_badge.set_visible(false);
        overlay.add_overlay(&video_badge);

        // Add click handler to the overlay
        let row_widget = self.clone();
        let click = GestureClick::new();
        click.set_button(1);
        click.connect_pressed(move |_, _n, _x, _y| {
            row_widget.emit_item_activated(index);
        });
        overlay.add_controller(click);

        // Add right-click context menu handler
        let row_widget = self.clone();
        let overlay_widget: Widget = overlay.clone().upcast();
        let context_click = GestureClick::new();
        context_click.set_button(3);
        context_click.connect_pressed(move |_, _n, x, y| {
            let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
            row_widget.emit_item_context_menu(index, &overlay_widget, rect);
        });
        overlay.add_controller(context_click);

        imp::ItemSlot {
            widget: overlay.clone().upcast(),
            picture,
            overlay: Some(overlay),
            label: Some(label),
            video_badge: Some(video_badge),
        }
    }

    fn emit_item_activated(&self, index: u32) {
        let imp = self.imp();
        let row = imp.row_index.get();
        if let Some(path) = imp.item_paths.borrow().get(index as usize).cloned() {
            if let Some(ref callback) = *imp.on_item_activated.borrow() {
                callback(row, index, path);
            }
        }
    }

    fn emit_item_context_menu(&self, index: u32, anchor: &Widget, rect: gdk::Rectangle) {
        let imp = self.imp();
        let row = imp.row_index.get();
        if let Some(path) = imp.item_paths.borrow().get(index as usize).cloned() {
            if let Some(ref callback) = *imp.on_item_context_menu.borrow() {
                callback(row, index, path, anchor.clone(), rect);
            }
        }
    }
}

impl Default for RowWidget {
    fn default() -> Self {
        Self::new()
    }
}

// Factory for creating RowWidget instances
// This provides a simple way to get/recycle row widgets
#[cfg(test)]
struct RowWidgetFactory {
    pool: RefCell<Vec<RowWidget>>,
    max_pool_size: usize,
}

#[cfg(test)]
impl RowWidgetFactory {
    fn new(max_pool_size: usize) -> Self {
        Self {
            pool: RefCell::new(Vec::with_capacity(max_pool_size)),
            max_pool_size,
        }
    }

    /// Get a row widget from the pool or create a new one
    fn get(&self) -> RowWidget {
        self.pool.borrow_mut().pop().unwrap_or_else(RowWidget::new)
    }

    /// Return a row widget to the pool for reuse
    fn recycle(&self, widget: RowWidget) {
        widget.unbind();
        let mut pool = self.pool.borrow_mut();
        if pool.len() < self.max_pool_size {
            pool.push(widget);
        }
        // If pool is full, widget is dropped
    }

    /// Clear the pool
    fn clear(&self) {
        self.pool.borrow_mut().clear();
    }
}

#[cfg(test)]
impl Default for RowWidgetFactory {
    fn default() -> Self {
        Self::new(50) // Default pool size of 50 widgets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::RowItem;
    use std::path::PathBuf;

    #[test]
    fn test_placeholder_texture() {
        // This test requires GTK initialization, skip in unit tests
        // gtk4::init().ok();
        // let texture = placeholder_texture();
        // assert!(texture.width() > 0);
    }
}
