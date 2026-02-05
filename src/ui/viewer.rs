// Image viewer overlay for idxd media browser
// Features:
// - Fast preview using thumbnail, then background-load full resolution
// - Zoom/pan with smooth scaling (GestureZoom, GestureDrag)
// - Overlay mode covering the main grid view
// - Terminal aesthetic: no rounded corners, no shadows, outlined buttons

use gdk4::{MemoryFormat, MemoryTexture, Rectangle, Texture};
use gtk4::gdk::Key;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{
    glib, Align, Box as GtkBox, Button, EventControllerKey, EventControllerMotion,
    EventControllerScroll, EventControllerScrollFlags, Fixed, GestureClick, GestureDrag,
    GestureZoom, GraphicsOffloadEnabled, Label, MediaFile, MediaStream, Orientation, Overlay,
    Picture, Scale, Stack, StackTransitionType, Video, Widget, Window,
};
use image::GenericImageView;
use lru::LruCache;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Maximum zoom scale allowed
const MAX_SCALE: f64 = 10.0;
/// Minimum zoom scale allowed
const MIN_SCALE: f64 = 0.1;
/// Zoom step for scroll wheel
const SCROLL_ZOOM_FACTOR: f64 = 0.1;
/// Ignore tiny touchpad jitter deltas that cause direction flapping.
const SCROLL_DEADZONE: f64 = 0.02;
/// Logical scroll units needed to trigger one zoom step.
const SCROLL_STEP_UNIT: f64 = 0.5;
/// Target size for fast preview decode (pixels on longest side)
const PREVIEW_SIZE: u32 = 512;
const DEFAULT_PREFETCH_MB: usize = 256;

fn video_offload_enabled() -> bool {
    std::env::var("IDXD_VIDEO_OFFLOAD")
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn prefetch_cache_bytes() -> usize {
    std::env::var("IDXD_PREFETCH_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(DEFAULT_PREFETCH_MB * 1024 * 1024)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefetchKind {
    Preview,
    Full,
}

#[derive(Clone)]
pub struct PrefetchItem {
    pub path: PathBuf,
    pub kind: PrefetchKind,
}

/// Result of background image loading - must be Send for cross-thread transfer
pub(crate) struct LoadResult {
    data: Vec<u8>,
    width: u32,
    height: u32,
    orig_width: u32,
    orig_height: u32,
    is_preview: bool,
}

pub(super) struct PrefetchResult {
    path: PathBuf,
    data: Vec<u8>,
    width: u32,
    height: u32,
    orig_width: u32,
    orig_height: u32,
    kind: PrefetchKind,
}

#[derive(Clone)]
pub(super) struct CachedTexture {
    texture: Texture,
    orig_width: u32,
    orig_height: u32,
    bytes: usize,
    kind: PrefetchKind,
}

pub(super) struct TextureCache {
    max_bytes: usize,
    bytes: usize,
    entries: LruCache<PathBuf, CachedTexture>,
}

impl TextureCache {
    fn new(max_bytes: usize) -> Self {
        let capacity = NonZeroUsize::new(2048).unwrap();
        Self {
            max_bytes,
            bytes: 0,
            entries: LruCache::new(capacity),
        }
    }

    fn get(&mut self, path: &Path) -> Option<CachedTexture> {
        self.entries.get(path).cloned()
    }

    fn insert(&mut self, path: PathBuf, entry: CachedTexture) {
        if let Some(existing) = self.entries.peek(&path) {
            if existing.kind == PrefetchKind::Full && entry.kind == PrefetchKind::Preview {
                return;
            }
        }

        if let Some(existing) = self.entries.put(path, entry.clone()) {
            self.bytes = self.bytes.saturating_sub(existing.bytes);
        }
        self.bytes = self.bytes.saturating_add(entry.bytes);

        while self.bytes > self.max_bytes {
            if let Some((_key, evicted)) = self.entries.pop_lru() {
                self.bytes = self.bytes.saturating_sub(evicted.bytes);
            } else {
                break;
            }
        }
    }

    fn contains(&mut self, path: &Path) -> bool {
        self.entries.get(path).is_some()
    }
}

// GObject subclass for MediaViewer
mod imp {
    use super::*;

    pub struct MediaViewerInner {
        // Main overlay container
        pub overlay: RefCell<Option<Overlay>>,
        // Content stack to switch between image and video rendering paths
        pub content_stack: RefCell<Option<Stack>>,
        // Fixed container for positioning the picture
        pub fixed: RefCell<Option<Fixed>>,
        // The picture widget displaying the image
        pub picture: RefCell<Option<Picture>>,
        // Embedded video area
        pub video_area: RefCell<Option<Video>>,
        pub video_stream: RefCell<Option<MediaFile>>,
        pub video_timer: RefCell<Option<glib::SourceId>>,
        // Current image path
        pub current_path: RefCell<Option<PathBuf>>,
        // Whether current content is video
        pub is_video: Cell<bool>,
        // Current zoom scale
        pub scale: Cell<f64>,
        // Pan offset
        pub pan_x: Cell<f64>,
        pub pan_y: Cell<f64>,
        // Image dimensions (original)
        pub image_width: Cell<u32>,
        pub image_height: Cell<u32>,
        // Is viewer visible
        pub visible: Cell<bool>,
        // Controls bar
        pub controls: RefCell<Option<GtkBox>>,
        pub image_controls: RefCell<Option<GtkBox>>,
        pub video_controls: RefCell<Option<GtkBox>>,
        pub video_play_btn: RefCell<Option<Button>>,
        pub video_seek_scale: RefCell<Option<Scale>>,
        pub video_seek_syncing: Cell<bool>,
        // Info label
        pub info_label: RefCell<Option<Label>>,
        // Zoom label
        pub zoom_label: RefCell<Option<Label>>,
        // Loading state
        pub is_loading: Cell<bool>,
        // Track if user interacted (zoom/pan) to avoid overriding scale
        pub user_interacted: Cell<bool>,
        // Closure to call when viewer is closed
        pub on_close: RefCell<Option<Rc<dyn Fn()>>>,
        // Context menu callback
        pub on_context_menu: RefCell<Option<Rc<dyn Fn(PathBuf, Widget, Rectangle)>>>,
        // Loading generation counter (to ignore stale results)
        pub load_generation: Cell<u64>,
        pub load_generation_atomic: Arc<AtomicU64>,
        // Channel sender for image load results (wrapped in Rc for sharing)
        pub(crate) load_sender: RefCell<Option<async_channel::Sender<(u64, LoadResult)>>>,
        pub(super) prefetch_sender: RefCell<Option<async_channel::Sender<PrefetchResult>>>,
        pub(super) prefetch_request_tx: RefCell<Option<flume::Sender<PrefetchItem>>>,
        pub(super) prefetch_pending: Arc<Mutex<HashSet<PathBuf>>>,
        pub(super) preview_cache: RefCell<TextureCache>,
        // Track pointer position for zoom-at-cursor
        pub pointer_x: Cell<f64>,
        pub pointer_y: Cell<f64>,
        // Track pan at drag start
        pub drag_start_pan_x: Cell<f64>,
        pub drag_start_pan_y: Cell<f64>,
        // Fractional scroll accumulator for stable wheel/touchpad zoom stepping.
        pub scroll_accum: Cell<f64>,
        // Last transform to avoid redundant GTK relayout work during drag/zoom.
        pub last_req_w: Cell<i32>,
        pub last_req_h: Cell<i32>,
        pub last_pos_x: Cell<f64>,
        pub last_pos_y: Cell<f64>,
    }

    impl Default for MediaViewerInner {
        fn default() -> Self {
            Self {
                overlay: RefCell::new(None),
                content_stack: RefCell::new(None),
                fixed: RefCell::new(None),
                picture: RefCell::new(None),
                video_area: RefCell::new(None),
                video_stream: RefCell::new(None),
                video_timer: RefCell::new(None),
                current_path: RefCell::new(None),
                is_video: Cell::new(false),
                scale: Cell::new(1.0),
                pan_x: Cell::new(0.0),
                pan_y: Cell::new(0.0),
                image_width: Cell::new(0),
                image_height: Cell::new(0),
                visible: Cell::new(false),
                controls: RefCell::new(None),
                image_controls: RefCell::new(None),
                video_controls: RefCell::new(None),
                video_play_btn: RefCell::new(None),
                video_seek_scale: RefCell::new(None),
                video_seek_syncing: Cell::new(false),
                info_label: RefCell::new(None),
                zoom_label: RefCell::new(None),
                is_loading: Cell::new(false),
                user_interacted: Cell::new(false),
                on_close: RefCell::new(None),
                on_context_menu: RefCell::new(None),
                load_generation: Cell::new(0),
                load_generation_atomic: Arc::new(AtomicU64::new(0)),
                load_sender: RefCell::new(None),
                prefetch_sender: RefCell::new(None),
                prefetch_request_tx: RefCell::new(None),
                prefetch_pending: Arc::new(Mutex::new(HashSet::new())),
                preview_cache: RefCell::new(TextureCache::new(prefetch_cache_bytes())),
                pointer_x: Cell::new(0.0),
                pointer_y: Cell::new(0.0),
                drag_start_pan_x: Cell::new(0.0),
                drag_start_pan_y: Cell::new(0.0),
                scroll_accum: Cell::new(0.0),
                last_req_w: Cell::new(-1),
                last_req_h: Cell::new(-1),
                last_pos_x: Cell::new(f64::NAN),
                last_pos_y: Cell::new(f64::NAN),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MediaViewerInner {
        const NAME: &'static str = "IdxdMediaViewer";
        type Type = super::MediaViewer;
        type ParentType = glib::Object;
    }

    impl ObjectImpl for MediaViewerInner {}
}

glib::wrapper! {
    pub struct MediaViewer(ObjectSubclass<imp::MediaViewerInner>);
}

impl MediaViewer {
    pub fn new() -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.setup_channels();
        obj.setup_widgets();
        obj
    }

    /// Set up async channels for background loading
    fn setup_channels(&self) {
        let imp = self.imp();

        // Create unbounded channel for load results
        let (sender, receiver) = async_channel::unbounded::<(u64, LoadResult)>();
        *imp.load_sender.borrow_mut() = Some(sender);

        // Set up receiver to process results on main thread using glib's async
        let viewer_weak = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok((generation, result)) = receiver.recv().await {
                if let Some(viewer) = viewer_weak.upgrade() {
                    viewer.handle_load_result(generation, result);
                } else {
                    // Viewer was dropped, exit the loop
                    break;
                }
            }
        });

        // Create unbounded channel for prefetch results
        let (prefetch_sender, prefetch_receiver) = async_channel::unbounded::<PrefetchResult>();
        *imp.prefetch_sender.borrow_mut() = Some(prefetch_sender.clone());

        let viewer_weak = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(result) = prefetch_receiver.recv().await {
                if let Some(viewer) = viewer_weak.upgrade() {
                    viewer.handle_prefetch_result(result);
                } else {
                    break;
                }
            }
        });

        // Bounded worker queue for prefetch decode to avoid spawning threads per selection change.
        let (prefetch_req_tx, prefetch_req_rx) = flume::bounded::<PrefetchItem>(256);
        *imp.prefetch_request_tx.borrow_mut() = Some(prefetch_req_tx);

        for _ in 0..2 {
            let rx = prefetch_req_rx.clone();
            let sender = prefetch_sender.clone();
            let pending = imp.prefetch_pending.clone();
            std::thread::spawn(move || {
                while let Ok(item) = rx.recv() {
                    let decoded = match item.kind {
                        PrefetchKind::Preview => decode_image_downscaled(&item.path, PREVIEW_SIZE)
                            .map(|(data, w, h, ow, oh)| (data, w, h, ow, oh)),
                        PrefetchKind::Full => {
                            decode_image_full(&item.path).map(|(data, w, h)| (data, w, h, w, h))
                        }
                    };

                    if let Some((data, width, height, orig_width, orig_height)) = decoded {
                        let _ = sender.send_blocking(PrefetchResult {
                            path: item.path.clone(),
                            data,
                            width,
                            height,
                            orig_width,
                            orig_height,
                            kind: item.kind,
                        });
                    }

                    if let Ok(mut in_flight) = pending.lock() {
                        in_flight.remove(&item.path);
                    }
                }
            });
        }
    }

    /// Handle a load result from the background thread
    fn handle_load_result(&self, generation: u64, result: LoadResult) {
        let imp = self.imp();

        // Check if this result is still relevant (generation matches and viewer is visible)
        if generation != imp.load_generation.get() || !imp.visible.get() {
            return;
        }

        // Ignore stale preview if a full-res image has already been applied.
        if result.is_preview && !imp.is_loading.get() {
            return;
        }

        // Create texture from the loaded data
        if let Some(texture) =
            Self::create_texture_from_rgba(&result.data, result.width, result.height)
        {
            self.set_texture(Some(&texture));
            imp.image_width.set(result.orig_width);
            imp.image_height.set(result.orig_height);

            if let Some(path) = imp.current_path.borrow().clone() {
                let kind = if result.is_preview {
                    PrefetchKind::Preview
                } else {
                    PrefetchKind::Full
                };
                self.cache_insert(
                    path,
                    texture.clone(),
                    result.width,
                    result.height,
                    result.orig_width,
                    result.orig_height,
                    kind,
                );
            }

            if !result.is_preview {
                imp.is_loading.set(false);
                self.set_preview_loading(false);
            } else {
                self.set_preview_loading(true);
            }

            if !imp.user_interacted.get() {
                self.fit_to_window();
            }

            self.update_info_label(
                Some(result.orig_width),
                Some(result.orig_height),
                result.is_preview,
            );

            // If the viewer was not allocated yet when this frame landed, retry layout shortly.
            self.schedule_layout_retry(generation);
        }
    }

    fn handle_prefetch_result(&self, result: PrefetchResult) {
        if let Some(texture) =
            Self::create_texture_from_rgba(&result.data, result.width, result.height)
        {
            self.cache_insert(
                result.path,
                texture,
                result.width,
                result.height,
                result.orig_width,
                result.orig_height,
                result.kind,
            );
        }
    }

    /// Set up the viewer widgets
    fn setup_widgets(&self) {
        let imp = self.imp();

        // Create the main overlay
        let overlay = Overlay::new();
        overlay.set_hexpand(true);
        overlay.set_vexpand(true);
        overlay.add_css_class("viewer-overlay");
        overlay.set_visible(false);

        // Create a Fixed container for absolute positioning
        let fixed = Fixed::new();
        fixed.set_hexpand(true);
        fixed.set_vexpand(true);

        let content_stack = Stack::new();
        content_stack.set_hexpand(true);
        content_stack.set_vexpand(true);
        // Size to the currently visible page so a large hidden image request
        // cannot blow up the viewer allocation (and therefore video layout).
        content_stack.set_hhomogeneous(false);
        content_stack.set_vhomogeneous(false);
        content_stack.set_transition_type(StackTransitionType::None);

        // Create the picture widget for displaying images
        let picture = Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk4::ContentFit::Fill);
        picture.add_css_class("viewer-image");

        // Create embedded video area
        let video_area = Video::new();
        video_area.set_autoplay(true);
        video_area.set_loop(false);
        // Offload can mis-size video surfaces on some Wayland compositors (notably Hyprland).
        // Keep it opt-in until sizing behavior is reliable.
        video_area.set_graphics_offload(if video_offload_enabled() {
            GraphicsOffloadEnabled::Enabled
        } else {
            GraphicsOffloadEnabled::Disabled
        });
        video_area.set_hexpand(true);
        video_area.set_vexpand(true);
        video_area.set_halign(Align::Fill);
        video_area.set_valign(Align::Fill);
        video_area.set_overflow(gtk4::Overflow::Hidden);
        video_area.add_css_class("viewer-video");
        video_area.set_visible(true);

        // Add picture to fixed at initial position (0,0)
        fixed.put(&picture, 0.0, 0.0);
        content_stack.add_named(&fixed, Some("image"));
        content_stack.add_named(&video_area, Some("video"));
        content_stack.set_visible_child_name("image");

        // Create controls bar at bottom
        let controls = GtkBox::new(Orientation::Horizontal, 8);
        controls.set_halign(Align::Fill);
        controls.set_valign(Align::End);
        controls.add_css_class("viewer-controls");
        controls.set_margin_start(8);
        controls.set_margin_end(8);
        controls.set_margin_bottom(8);

        // Close button
        let close_btn = Button::with_label("[X] CLOSE");
        close_btn.add_css_class("btn-primary");
        close_btn.set_tooltip_text(Some("Close viewer (Escape)"));

        // Zoom info
        let zoom_label = Label::new(Some("100%"));
        zoom_label.add_css_class("muted");
        zoom_label.set_width_chars(6);

        // Zoom in/out buttons
        let zoom_in_btn = Button::with_label("[+]");
        zoom_in_btn.set_tooltip_text(Some("Zoom in"));

        let zoom_out_btn = Button::with_label("[-]");
        zoom_out_btn.set_tooltip_text(Some("Zoom out"));

        // Fit to window button
        let fit_btn = Button::with_label("[FIT]");
        fit_btn.set_tooltip_text(Some("Fit to window"));

        // 1:1 scale button
        let actual_btn = Button::with_label("[1:1]");
        actual_btn.set_tooltip_text(Some("Actual size"));

        // Video controls
        let seek_back_btn = Button::with_label("[<< 5s]");
        seek_back_btn.set_tooltip_text(Some("Seek backward 5 seconds"));

        let play_pause_btn = Button::with_label("[PAUSE]");
        play_pause_btn.set_tooltip_text(Some("Play/Pause (Space)"));

        let seek_fwd_btn = Button::with_label("[5s >>]");
        seek_fwd_btn.set_tooltip_text(Some("Seek forward 5 seconds"));

        let seek_scale = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 0.1);
        seek_scale.set_hexpand(true);
        seek_scale.set_draw_value(false);
        seek_scale.set_sensitive(false);
        seek_scale.set_tooltip_text(Some("Seek within video"));

        // Info label (filename, dimensions)
        let info_label = Label::new(None);
        info_label.set_halign(Align::Start);
        info_label.set_hexpand(true);
        info_label.add_css_class("muted");
        info_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);

        let image_controls = GtkBox::new(Orientation::Horizontal, 8);
        image_controls.append(&zoom_out_btn);
        image_controls.append(&zoom_label);
        image_controls.append(&zoom_in_btn);
        image_controls.append(&fit_btn);
        image_controls.append(&actual_btn);

        let video_controls = GtkBox::new(Orientation::Horizontal, 8);
        video_controls.append(&seek_back_btn);
        video_controls.append(&play_pause_btn);
        video_controls.append(&seek_scale);
        video_controls.append(&seek_fwd_btn);
        video_controls.set_visible(false);

        // Add controls to bar
        controls.append(&close_btn);
        controls.append(&gtk4::Separator::new(Orientation::Vertical));
        controls.append(&image_controls);
        controls.append(&video_controls);
        controls.append(&gtk4::Separator::new(Orientation::Vertical));
        controls.append(&info_label);

        // Set up overlay with stack as main child
        overlay.set_child(Some(&content_stack));
        overlay.add_overlay(&controls);

        // Store references
        *imp.overlay.borrow_mut() = Some(overlay.clone());
        *imp.content_stack.borrow_mut() = Some(content_stack.clone());
        *imp.fixed.borrow_mut() = Some(fixed.clone());
        *imp.picture.borrow_mut() = Some(picture.clone());
        *imp.video_area.borrow_mut() = Some(video_area.clone());
        *imp.controls.borrow_mut() = Some(controls);
        *imp.image_controls.borrow_mut() = Some(image_controls.clone());
        *imp.video_controls.borrow_mut() = Some(video_controls.clone());
        *imp.video_play_btn.borrow_mut() = Some(play_pause_btn.clone());
        *imp.video_seek_scale.borrow_mut() = Some(seek_scale.clone());
        *imp.info_label.borrow_mut() = Some(info_label.clone());
        *imp.zoom_label.borrow_mut() = Some(zoom_label.clone());
        imp.scale.set(1.0);

        // Set up gestures
        self.setup_gestures(&picture, &overlay, &fixed);

        // Set up keyboard controls
        self.setup_keyboard(&overlay);

        // Connect button signals
        let viewer_weak = self.downgrade();
        close_btn.connect_clicked(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.hide();
            }
        });

        let viewer_weak = self.downgrade();
        zoom_in_btn.connect_clicked(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.zoom_by(1.25);
            }
        });

        let viewer_weak = self.downgrade();
        zoom_out_btn.connect_clicked(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.zoom_by(0.8);
            }
        });

        let viewer_weak = self.downgrade();
        fit_btn.connect_clicked(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.fit_to_window();
            }
        });

        let viewer_weak = self.downgrade();
        actual_btn.connect_clicked(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.set_scale(1.0);
            }
        });

        let viewer_weak = self.downgrade();
        seek_back_btn.connect_clicked(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.seek_video_relative(-5.0);
            }
        });

        let viewer_weak = self.downgrade();
        play_pause_btn.connect_clicked(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.toggle_video_play_pause();
            }
        });

        let viewer_weak = self.downgrade();
        seek_fwd_btn.connect_clicked(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.seek_video_relative(5.0);
            }
        });

        let viewer_weak = self.downgrade();
        seek_scale.connect_value_changed(move |scale| {
            let Some(viewer) = viewer_weak.upgrade() else {
                return;
            };
            let imp = viewer.imp();
            if imp.video_seek_syncing.get() || !imp.is_video.get() {
                return;
            }
            let stream_ref = imp.video_stream.borrow();
            let Some(stream) = stream_ref.as_ref() else {
                return;
            };
            if !stream.is_seekable() {
                return;
            }
            let duration = (stream.duration() as f64) / 1_000_000.0;
            if duration <= 0.0 {
                return;
            }
            let target_seconds = scale.value().clamp(0.0, duration);
            let target = (target_seconds * 1_000_000.0).round() as i64;
            stream.seek(target);
        });
    }

    /// Set up zoom and drag gestures
    fn setup_gestures(&self, _picture: &Picture, overlay: &Overlay, fixed: &Fixed) {
        // Track pointer position on the fixed container (same coord space as picture positioning)
        let motion_controller = EventControllerMotion::new();
        let viewer_weak = self.downgrade();
        motion_controller.connect_motion(move |_, x, y| {
            if let Some(viewer) = viewer_weak.upgrade() {
                let imp = viewer.imp();
                imp.pointer_x.set(x);
                imp.pointer_y.set(y);
            }
        });
        let viewer_weak = self.downgrade();
        motion_controller.connect_enter(move |_, x, y| {
            if let Some(viewer) = viewer_weak.upgrade() {
                let imp = viewer.imp();
                imp.pointer_x.set(x);
                imp.pointer_y.set(y);
            }
        });
        fixed.add_controller(motion_controller);

        // Zoom gesture (pinch) on overlay
        let zoom_gesture = GestureZoom::new();
        let viewer_weak = self.downgrade();
        zoom_gesture.connect_scale_changed(move |_gesture, scale| {
            if let Some(viewer) = viewer_weak.upgrade() {
                if viewer.imp().is_video.get() {
                    return;
                }
                let base_scale = viewer.imp().scale.get();
                let new_scale = (base_scale * scale).clamp(MIN_SCALE, MAX_SCALE);
                viewer.set_scale(new_scale);
            }
        });
        overlay.add_controller(zoom_gesture);

        // Drag gesture (pan) on overlay - only with mouse button 1 (left click)
        let drag_gesture = GestureDrag::new();
        drag_gesture.set_button(1); // Left mouse button only

        let viewer_weak = self.downgrade();
        drag_gesture.connect_drag_begin(move |_, _x, _y| {
            if let Some(viewer) = viewer_weak.upgrade() {
                let imp = viewer.imp();
                imp.drag_start_pan_x.set(imp.pan_x.get());
                imp.drag_start_pan_y.set(imp.pan_y.get());
            }
        });

        let viewer_weak = self.downgrade();
        drag_gesture.connect_drag_update(move |_, offset_x, offset_y| {
            if let Some(viewer) = viewer_weak.upgrade() {
                let imp = viewer.imp();
                if imp.is_video.get() {
                    return;
                }
                let start_x = imp.drag_start_pan_x.get();
                let start_y = imp.drag_start_pan_y.get();
                imp.pan_x.set(start_x + offset_x);
                imp.pan_y.set(start_y + offset_y);
                imp.user_interacted.set(true);
                viewer.update_transform();
            }
        });

        overlay.add_controller(drag_gesture);

        // Right-click context menu on overlay
        let context_click = GestureClick::new();
        context_click.set_button(3);
        let viewer_weak = self.downgrade();
        let overlay_widget: Widget = overlay.clone().upcast();
        context_click.connect_pressed(move |_, _n, x, y| {
            if let Some(viewer) = viewer_weak.upgrade() {
                let rect = Rectangle::new(x as i32, y as i32, 1, 1);
                viewer.emit_context_menu(&overlay_widget, rect);
            }
        });
        overlay.add_controller(context_click);

        // Scroll wheel for zoom on fixed container (same coord space as motion tracking)
        let scroll_controller = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
        let viewer_weak = self.downgrade();
        scroll_controller.connect_scroll(move |controller, _dx, dy| {
            if let Some(viewer) = viewer_weak.upgrade() {
                let imp = viewer.imp();
                if imp.is_video.get() {
                    return glib::Propagation::Proceed;
                }

                // Keep pointer tracking fresh even on tiny deltas so the next
                // accumulated zoom step does not use an older anchor.
                if let Some((px, py)) = controller
                    .current_event()
                    .and_then(|event| event.position())
                {
                    imp.pointer_x.set(px);
                    imp.pointer_y.set(py);
                }

                // Ignore no-op scroll deltas (some touchpads emit zero on axis-change frames).
                if dy.abs() < SCROLL_DEADZONE {
                    return glib::Propagation::Proceed;
                }

                // Prefer the pointer position from this exact scroll event to avoid stale cursor data.
                let (px, py) = controller
                    .current_event()
                    .and_then(|event| event.position())
                    .unwrap_or((imp.pointer_x.get(), imp.pointer_y.get()));

                imp.pointer_x.set(px);
                imp.pointer_y.set(py);
                let mut accum = imp.scroll_accum.get() + dy;
                let mut steps = 0i32;
                while accum.abs() >= SCROLL_STEP_UNIT && steps.abs() < 16 {
                    if accum > 0.0 {
                        steps += 1;
                        accum -= SCROLL_STEP_UNIT;
                    } else {
                        steps -= 1;
                        accum += SCROLL_STEP_UNIT;
                    }
                }
                imp.scroll_accum.set(accum);

                if steps == 0 {
                    return glib::Propagation::Proceed;
                }

                let step_factor = 1.0 + SCROLL_ZOOM_FACTOR;
                let factor = step_factor.powi(-steps);
                viewer.zoom_at_point_with_factor(px, py, factor);
            }
            glib::Propagation::Stop
        });
        fixed.add_controller(scroll_controller);
    }

    /// Set up keyboard controls
    fn setup_keyboard(&self, overlay: &Overlay) {
        let key_controller = EventControllerKey::new();
        let viewer_weak = self.downgrade();

        key_controller.connect_key_pressed(move |_, key, _code, _state| {
            if let Some(viewer) = viewer_weak.upgrade() {
                match key {
                    Key::Escape | Key::q => {
                        viewer.hide();
                        glib::Propagation::Stop
                    }
                    Key::space => {
                        if viewer.imp().is_video.get() {
                            viewer.toggle_video_play_pause();
                            glib::Propagation::Stop
                        } else {
                            glib::Propagation::Proceed
                        }
                    }
                    Key::plus | Key::equal | Key::KP_Add => {
                        viewer.zoom_by(1.25);
                        glib::Propagation::Stop
                    }
                    Key::minus | Key::KP_Subtract => {
                        viewer.zoom_by(0.8);
                        glib::Propagation::Stop
                    }
                    Key::_0 | Key::KP_0 => {
                        viewer.fit_to_window();
                        glib::Propagation::Stop
                    }
                    Key::_1 | Key::KP_1 => {
                        viewer.set_scale(1.0);
                        glib::Propagation::Stop
                    }
                    Key::Left | Key::h => {
                        if viewer.imp().is_video.get() {
                            viewer.seek_video_relative(-5.0);
                        } else {
                            viewer.pan_by(-50.0, 0.0);
                        }
                        glib::Propagation::Stop
                    }
                    Key::Right | Key::l => {
                        if viewer.imp().is_video.get() {
                            viewer.seek_video_relative(5.0);
                        } else {
                            viewer.pan_by(50.0, 0.0);
                        }
                        glib::Propagation::Stop
                    }
                    Key::Up | Key::k => {
                        viewer.pan_by(0.0, -50.0);
                        glib::Propagation::Stop
                    }
                    Key::Down | Key::j => {
                        viewer.pan_by(0.0, 50.0);
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            } else {
                glib::Propagation::Proceed
            }
        });

        overlay.add_controller(key_controller);
    }

    /// Get the widget to add to the UI
    pub fn widget(&self) -> Widget {
        self.imp()
            .overlay
            .borrow()
            .as_ref()
            .unwrap()
            .clone()
            .upcast()
    }

    fn set_video_mode(&self, is_video: bool) {
        let imp = self.imp();
        if is_video {
            // Clear any zoom-sized request from the image widget so it does not
            // affect stack measurement while video is active.
            if let Some(picture) = imp.picture.borrow().as_ref() {
                picture.set_size_request(-1, -1);
            }
            imp.last_req_w.set(-1);
            imp.last_req_h.set(-1);
            imp.last_pos_x.set(f64::NAN);
            imp.last_pos_y.set(f64::NAN);
        }
        if let Some(image_controls) = imp.image_controls.borrow().as_ref() {
            image_controls.set_visible(!is_video);
        }
        if let Some(video_controls) = imp.video_controls.borrow().as_ref() {
            video_controls.set_visible(is_video);
        }
        if let Some(picture) = imp.picture.borrow().as_ref() {
            picture.set_visible(!is_video);
        }
        if let Some(video_area) = imp.video_area.borrow().as_ref() {
            video_area.set_visible(is_video);
        }
        if let Some(stack) = imp.content_stack.borrow().as_ref() {
            stack.set_visible_child_name(if is_video { "video" } else { "image" });
        }
    }

    fn update_video_layout(&self) {
        if let Some(video_area) = self.imp().video_area.borrow().as_ref() {
            // Let GTK allocate naturally with expand+fill; forcing size requests can
            // result in clipped/zoomed output for dynamic paintables on some backends.
            video_area.set_size_request(-1, -1);
            video_area.queue_allocate();
            video_area.queue_draw();
        }
        self.log_video_debug("update_video_layout");
    }

    fn schedule_video_layout_retry(&self, generation: u64) {
        let viewer_weak = self.downgrade();
        let mut attempts = 0u8;
        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            attempts = attempts.saturating_add(1);
            let Some(viewer) = viewer_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let imp = viewer.imp();

            if generation != imp.load_generation.get() || !imp.visible.get() || !imp.is_video.get()
            {
                return glib::ControlFlow::Break;
            }

            let (w, h) = if let Some(overlay) = imp.overlay.borrow().as_ref() {
                (overlay.width(), overlay.height())
            } else {
                (0, 0)
            };
            tracing::info!(
                "video-debug retry attempt={} overlay={}x{} visible={} is_video={}",
                attempts,
                w,
                h,
                imp.visible.get(),
                imp.is_video.get()
            );

            if w > 1 && h > 1 {
                viewer.update_video_layout();
                viewer.log_video_debug("schedule_video_layout_retry:ready");
                return glib::ControlFlow::Break;
            }

            if attempts >= 60 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    fn start_video_info_timer(&self) {
        self.stop_video_info_timer();
        let viewer_weak = self.downgrade();
        let source_id = glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
            let Some(viewer) = viewer_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let imp = viewer.imp();
            if !imp.visible.get() || !imp.is_video.get() {
                return glib::ControlFlow::Break;
            }
            viewer.update_video_info(0.0, 0.0);
            glib::ControlFlow::Continue
        });
        *self.imp().video_timer.borrow_mut() = Some(source_id);
    }

    fn stop_video_info_timer(&self) {
        if let Some(source_id) = self.imp().video_timer.borrow_mut().take() {
            source_id.remove();
        }
    }

    fn update_video_play_button(&self, playing: bool) {
        if let Some(play_btn) = self.imp().video_play_btn.borrow().as_ref() {
            let label = if playing { "[PAUSE]" } else { "[PLAY]" };
            play_btn.set_label(label);
        }
    }

    fn update_video_info(&self, position: f64, duration: f64) {
        let imp = self.imp();
        if !imp.is_video.get() {
            return;
        }
        let (position, duration, playing) = if let Some(stream) = imp.video_stream.borrow().as_ref()
        {
            (
                (stream.timestamp() as f64) / 1_000_000.0,
                (stream.duration() as f64) / 1_000_000.0,
                stream.is_playing(),
            )
        } else {
            (position, duration, false)
        };
        if let Some(label) = imp.info_label.borrow().as_ref() {
            let filename = imp
                .current_path
                .borrow()
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown".to_string());

            let pos = format_timestamp(position);
            let dur = format_timestamp(duration);
            if duration > 0.0 {
                label.set_text(&format!("> {} [{} / {}]", filename, pos, dur));
            } else {
                label.set_text(&format!("> {} [{}]", filename, pos));
            }
        }
        if let Some(zoom_label) = imp.zoom_label.borrow().as_ref() {
            zoom_label.set_text("VID");
        }
        if let Some(seek_scale) = imp.video_seek_scale.borrow().as_ref() {
            let seekable = duration > 0.0
                && imp
                    .video_stream
                    .borrow()
                    .as_ref()
                    .map(|s| s.is_seekable())
                    .unwrap_or(false);
            imp.video_seek_syncing.set(true);
            seek_scale.set_range(0.0, duration.max(1.0));
            seek_scale.set_sensitive(seekable);
            seek_scale.set_value(position.clamp(0.0, duration.max(0.0)));
            imp.video_seek_syncing.set(false);
        }
        self.update_video_play_button(playing);
    }

    fn log_video_debug(&self, context: &str) {
        let imp = self.imp();
        let (overlay_w, overlay_h, overlay_scale) = if let Some(overlay) = imp.overlay.borrow().as_ref() {
            (overlay.width(), overlay.height(), overlay.scale_factor())
        } else {
            (0, 0, 0)
        };
        let (stack_w, stack_h, stack_scale) = if let Some(stack) = imp.content_stack.borrow().as_ref() {
            (stack.width(), stack.height(), stack.scale_factor())
        } else {
            (0, 0, 0)
        };
        let (video_w, video_h, video_scale) = if let Some(video) = imp.video_area.borrow().as_ref() {
            (video.width(), video.height(), video.scale_factor())
        } else {
            (0, 0, 0)
        };
        let (stream_us, duration_us, prepared, has_video, playing, seekable, error_text) =
            if let Some(stream) = imp.video_stream.borrow().as_ref() {
                (
                    stream.timestamp(),
                    stream.duration(),
                    stream.is_prepared(),
                    stream.has_video(),
                    stream.is_playing(),
                    stream.is_seekable(),
                    stream.error().map(|e| e.message().to_string()),
                )
            } else {
                (0, 0, false, false, false, false, None)
            };
        let (root_w, root_h) = imp
            .overlay
            .borrow()
            .as_ref()
            .and_then(|overlay| overlay.root())
            .and_then(|root| root.downcast::<Window>().ok())
            .map(|w| (w.width(), w.height()))
            .unwrap_or((0, 0));
        tracing::info!(
            "video-debug {} root={}x{} overlay={}x{}@{} stack={}x{}@{} video_widget={}x{}@{} stream_ts_us={} duration_us={} prepared={} has_video={} playing={} seekable={} error={}",
            context,
            root_w,
            root_h,
            overlay_w,
            overlay_h,
            overlay_scale,
            stack_w,
            stack_h,
            stack_scale,
            video_w,
            video_h,
            video_scale,
            stream_us,
            duration_us,
            prepared,
            has_video,
            playing,
            seekable,
            error_text.as_deref().unwrap_or("none")
        );
    }

    fn attach_video_stream_debug(&self, media: &MediaFile) {
        let viewer_weak = self.downgrade();
        media.connect_prepared_notify(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.log_video_debug("signal:prepared");
            }
        });

        let viewer_weak = self.downgrade();
        media.connect_error_notify(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.log_video_debug("signal:error");
            }
        });

        let viewer_weak = self.downgrade();
        media.connect_has_video_notify(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.log_video_debug("signal:has-video");
            }
        });

        let viewer_weak = self.downgrade();
        media.connect_duration_notify(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.log_video_debug("signal:duration");
            }
        });

        let viewer_weak = self.downgrade();
        media.connect_playing_notify(move |_| {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.log_video_debug("signal:playing");
            }
        });
    }

    pub fn is_video_mode(&self) -> bool {
        self.imp().is_video.get()
    }

    pub fn toggle_video_play_pause(&self) {
        let imp = self.imp();
        if !imp.is_video.get() {
            return;
        }
        if let Some(stream) = imp.video_stream.borrow().as_ref() {
            if stream.is_playing() {
                stream.pause();
            } else {
                stream.play();
            }
            self.update_video_play_button(stream.is_playing());
        }
    }

    pub fn seek_video_relative(&self, seconds: f64) {
        let imp = self.imp();
        if !imp.is_video.get() {
            return;
        }
        if let Some(stream) = imp.video_stream.borrow().as_ref() {
            let current = stream.timestamp();
            let delta = (seconds * 1_000_000.0).round() as i64;
            let target = current.saturating_add(delta).max(0);
            stream.seek(target);
        }
    }

    /// Show the viewer with an image or video
    pub fn show(&self, image_path: &Path, thumbnail_path: Option<&Path>) {
        let imp = self.imp();

        // Increment generation to invalidate any pending loads
        let generation = imp.load_generation.get().wrapping_add(1);
        imp.load_generation.set(generation);
        imp.load_generation_atomic
            .store(generation, Ordering::SeqCst);
        let generation_guard = imp.load_generation_atomic.clone();

        // Reset state
        imp.scale.set(1.0);
        imp.pan_x.set(0.0);
        imp.pan_y.set(0.0);
        imp.last_req_w.set(-1);
        imp.last_req_h.set(-1);
        imp.last_pos_x.set(f64::NAN);
        imp.last_pos_y.set(f64::NAN);
        imp.scroll_accum.set(0.0);
        imp.visible.set(true);
        imp.is_video.set(is_video_path(image_path));
        imp.is_loading.set(!imp.is_video.get());
        self.set_preview_loading(!imp.is_video.get());
        imp.user_interacted.set(false);
        *imp.current_path.borrow_mut() = Some(image_path.to_path_buf());

        // Update info label
        if let Some(label) = imp.info_label.borrow().as_ref() {
            let filename = image_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown".to_string());
            label.set_text(&format!("> Loading: {}", filename));
        }

        // Show the overlay
        if let Some(overlay) = imp.overlay.borrow().as_ref() {
            overlay.set_visible(true);
            overlay.grab_focus();
        }
        if let Some(fixed) = imp.fixed.borrow().as_ref() {
            let cx = (fixed.width().max(1) as f64) * 0.5;
            let cy = (fixed.height().max(1) as f64) * 0.5;
            imp.pointer_x.set(cx);
            imp.pointer_y.set(cy);
        }

        if imp.is_video.get() {
            tracing::info!(
                "video-debug env session_type={} wayland_display={} hyprland_sig={} offload={}",
                std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "<unset>".to_string()),
                std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "<unset>".to_string()),
                std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
                    .map(|_| "<set>".to_string())
                    .unwrap_or_else(|_| "<unset>".to_string()),
                if video_offload_enabled() {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            self.set_video_mode(true);
            self.set_texture(Option::<&Texture>::None);
            if let Some(video) = imp.video_area.borrow().as_ref() {
                let media = MediaFile::for_filename(image_path);
                self.attach_video_stream_debug(&media);
                video.set_media_stream(Some(&media));
                media.play();
                *imp.video_stream.borrow_mut() = Some(media);
            }
            self.start_video_info_timer();
            self.update_video_play_button(true);
            self.update_video_info(0.0, 0.0);
            self.update_transform();
            self.log_video_debug("show:video-open");
            self.schedule_video_layout_retry(generation);
            return;
        }

        self.stop_video_info_timer();
        if let Some(video) = imp.video_area.borrow().as_ref() {
            video.set_media_stream(Option::<&MediaStream>::None);
        }
        if let Some(seek_scale) = imp.video_seek_scale.borrow().as_ref() {
            imp.video_seek_syncing.set(true);
            seek_scale.set_sensitive(false);
            seek_scale.set_range(0.0, 1.0);
            seek_scale.set_value(0.0);
            imp.video_seek_syncing.set(false);
        }
        *imp.video_stream.borrow_mut() = None;
        self.set_video_mode(false);
        self.schedule_layout_retry(generation);

        if let Some(cached) = self.cache_get(image_path) {
            self.set_texture(Some(&cached.texture));
            imp.image_width.set(cached.orig_width);
            imp.image_height.set(cached.orig_height);
            imp.is_loading.set(cached.kind == PrefetchKind::Preview);
            self.set_preview_loading(cached.kind == PrefetchKind::Preview);
            self.update_info_label(
                Some(cached.orig_width),
                Some(cached.orig_height),
                cached.kind == PrefetchKind::Preview,
            );
            if cached.kind == PrefetchKind::Full {
                self.fit_to_window();
                return;
            }
        }

        // Step 1: Show thumbnail immediately if available (fast preview)
        if let Some(thumb_path) = thumbnail_path {
            if let Some(texture) = self.load_texture_sync(thumb_path) {
                self.set_texture(Some(&texture));
            }
        }

        // Get the sender for background loading
        let sender = match imp.load_sender.borrow().as_ref() {
            Some(s) => s.clone(),
            None => return,
        };

        let image_path_owned = image_path.to_path_buf();

        // Step 2: Load downscaled preview quickly in background thread
        let sender_preview = sender.clone();
        let image_path_preview = image_path_owned.clone();
        let gen_preview = generation;
        let generation_guard_preview = generation_guard.clone();

        std::thread::spawn(move || {
            if gen_preview != generation_guard_preview.load(Ordering::SeqCst) {
                return;
            }
            if let Some((data, width, height, orig_w, orig_h)) =
                decode_image_downscaled(&image_path_preview, PREVIEW_SIZE)
            {
                let result = LoadResult {
                    data,
                    width,
                    height,
                    orig_width: orig_w,
                    orig_height: orig_h,
                    is_preview: true,
                };
                let _ = sender_preview.send_blocking((gen_preview, result));
            }
        });

        // Step 3: Load full resolution in background thread
        let sender_full = sender;
        let image_path_full = image_path_owned;
        let gen_full = generation;
        let generation_guard_full = generation_guard;

        std::thread::spawn(move || {
            if gen_full != generation_guard_full.load(Ordering::SeqCst) {
                return;
            }
            if let Some((data, width, height)) = decode_image_full(&image_path_full) {
                let result = LoadResult {
                    data,
                    width,
                    height,
                    orig_width: width,
                    orig_height: height,
                    is_preview: false,
                };
                let _ = sender_full.send_blocking((gen_full, result));
            }
        });
    }

    fn schedule_layout_retry(&self, generation: u64) {
        let viewer_weak = self.downgrade();
        let mut attempts = 0u8;
        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            attempts = attempts.saturating_add(1);
            let Some(viewer) = viewer_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let imp = viewer.imp();

            if generation != imp.load_generation.get() || !imp.visible.get() {
                return glib::ControlFlow::Break;
            }

            let (overlay_w, overlay_h) = if let Some(overlay) = imp.overlay.borrow().as_ref() {
                (overlay.width(), overlay.height())
            } else {
                (0, 0)
            };

            if overlay_w > 0
                && overlay_h > 0
                && imp.image_width.get() > 0
                && imp.image_height.get() > 0
            {
                if !imp.user_interacted.get() {
                    viewer.fit_to_window();
                } else {
                    viewer.update_transform();
                }
                return glib::ControlFlow::Break;
            }

            if attempts >= 30 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    pub fn prefetch(&self, mut items: Vec<PrefetchItem>) {
        let imp = self.imp();
        if items.is_empty() {
            return;
        }

        // Filter out items already in cache.
        {
            let mut cache = imp.preview_cache.borrow_mut();
            items.retain(|item| !is_video_path(&item.path) && !cache.contains(&item.path));
        }

        if items.is_empty() {
            return;
        }

        let tx = match imp.prefetch_request_tx.borrow().as_ref() {
            Some(s) => s.clone(),
            None => return,
        };

        for item in items {
            let should_queue = if let Ok(mut in_flight) = imp.prefetch_pending.lock() {
                in_flight.insert(item.path.clone())
            } else {
                true
            };
            if !should_queue {
                continue;
            }

            if tx.try_send(item.clone()).is_err() {
                if let Ok(mut in_flight) = imp.prefetch_pending.lock() {
                    in_flight.remove(&item.path);
                }
            }
        }
    }

    /// Load a texture synchronously from a path (for thumbnails)
    fn load_texture_sync(&self, path: &Path) -> Option<Texture> {
        // Try to load using GDK first (faster for supported formats)
        Texture::from_filename(path).ok()
    }

    /// Create a GDK texture from RGBA data
    fn create_texture_from_rgba(data: &[u8], width: u32, height: u32) -> Option<Texture> {
        if width == 0 || height == 0 {
            return None;
        }
        let expected = (width as u64)
            .saturating_mul(height as u64)
            .saturating_mul(4);
        if (data.len() as u64) < expected {
            tracing::warn!(
                "Skipping texture: data too small ({} bytes for {}x{})",
                data.len(),
                width,
                height
            );
            return None;
        }
        let bytes = glib::Bytes::from(data);
        let texture = MemoryTexture::new(
            width as i32,
            height as i32,
            MemoryFormat::R8g8b8a8,
            &bytes,
            (width * 4) as usize,
        );
        Some(texture.upcast())
    }

    /// Set the texture on the picture widget
    fn set_texture(&self, texture: Option<&Texture>) {
        if let Some(picture) = self.imp().picture.borrow().as_ref() {
            picture.set_paintable(texture);
        }
    }

    fn set_preview_loading(&self, loading: bool) {
        if let Some(picture) = self.imp().picture.borrow().as_ref() {
            if loading {
                picture.add_css_class("preview-loading");
            } else {
                picture.remove_css_class("preview-loading");
            }
        }
    }

    fn cache_get(&self, path: &Path) -> Option<CachedTexture> {
        self.imp().preview_cache.borrow_mut().get(path)
    }

    fn cache_insert(
        &self,
        path: PathBuf,
        texture: Texture,
        width: u32,
        height: u32,
        orig_width: u32,
        orig_height: u32,
        kind: PrefetchKind,
    ) {
        let bytes = (width as u64)
            .saturating_mul(height as u64)
            .saturating_mul(4) as usize;
        let entry = CachedTexture {
            texture,
            orig_width,
            orig_height,
            bytes,
            kind,
        };
        self.imp().preview_cache.borrow_mut().insert(path, entry);
    }

    /// Update the info label
    fn update_info_label(&self, width: Option<u32>, height: Option<u32>, is_preview: bool) {
        let imp = self.imp();
        if let Some(label) = imp.info_label.borrow().as_ref() {
            let path = imp.current_path.borrow();
            let filename = path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown".to_string());

            let dims = match (width, height) {
                (Some(w), Some(h)) => format!(" [{}x{}]", w, h),
                _ => String::new(),
            };

            let loading = if is_preview { " (preview)" } else { "" };
            let scale = imp.scale.get();

            label.set_text(&format!(
                "> {}{} @ {}%{}",
                filename,
                dims,
                (scale * 100.0) as i32,
                loading
            ));
        }

        // Update zoom label
        if let Some(zoom_label) = imp.zoom_label.borrow().as_ref() {
            let scale = imp.scale.get();
            zoom_label.set_text(&format!("{}%", (scale * 100.0) as i32));
        }
    }

    /// Hide the viewer
    pub fn hide(&self) {
        let imp = self.imp();

        // Increment generation to invalidate pending loads
        let generation = imp.load_generation.get().wrapping_add(1);
        imp.load_generation.set(generation);
        imp.load_generation_atomic
            .store(generation, Ordering::SeqCst);

        imp.visible.set(false);
        imp.is_loading.set(false);
        imp.is_video.set(false);
        imp.scroll_accum.set(0.0);
        self.set_preview_loading(false);
        self.set_video_mode(false);
        self.stop_video_info_timer();

        if let Some(video) = imp.video_area.borrow().as_ref() {
            video.set_media_stream(Option::<&MediaStream>::None);
        }
        *imp.video_stream.borrow_mut() = None;
        if let Some(picture) = imp.picture.borrow().as_ref() {
            picture.set_size_request(-1, -1);
        }
        imp.last_req_w.set(-1);
        imp.last_req_h.set(-1);
        imp.last_pos_x.set(f64::NAN);
        imp.last_pos_y.set(f64::NAN);

        if let Some(overlay) = imp.overlay.borrow().as_ref() {
            overlay.set_visible(false);
        }

        // Clear the picture to free memory
        self.set_texture(Option::<&Texture>::None);

        // Call the close callback
        if let Some(ref callback) = *imp.on_close.borrow() {
            callback();
        }
    }

    /// Check if the viewer is visible
    pub fn is_visible(&self) -> bool {
        self.imp().visible.get()
    }

    /// Set callback for when viewer is closed
    pub fn connect_close<F: Fn() + 'static>(&self, callback: F) {
        *self.imp().on_close.borrow_mut() = Some(Rc::new(callback));
    }

    /// Set callback for context menu requests
    pub fn connect_context_menu<F>(&self, callback: F)
    where
        F: Fn(PathBuf, Widget, Rectangle) + 'static,
    {
        *self.imp().on_context_menu.borrow_mut() = Some(Rc::new(callback));
    }

    /// Get the current path being displayed
    pub fn current_path(&self) -> Option<PathBuf> {
        self.imp().current_path.borrow().clone()
    }

    fn emit_context_menu(&self, anchor: &Widget, rect: Rectangle) {
        let imp = self.imp();
        let Some(path) = imp.current_path.borrow().clone() else {
            return;
        };
        if let Some(ref callback) = *imp.on_context_menu.borrow() {
            callback(path, anchor.clone(), rect);
        }
    }

    /// Zoom by a factor (1.0 = no change, 2.0 = double, 0.5 = half)
    pub fn zoom_by(&self, factor: f64) {
        let imp = self.imp();
        let current = imp.scale.get();
        let new_scale = (current * factor).clamp(MIN_SCALE, MAX_SCALE);
        self.set_scale_internal(new_scale, true);
    }

    /// Zoom toward or away from a specific point (for scroll wheel zoom)
    /// pointer_x, pointer_y are in Fixed container coordinate space.
    fn zoom_at_point_with_factor(&self, pointer_x: f64, pointer_y: f64, factor: f64) {
        let imp = self.imp();

        let old_scale = imp.scale.get();
        let new_scale = (old_scale * factor).clamp(MIN_SCALE, MAX_SCALE);

        if (new_scale - old_scale).abs() < 1e-9 {
            return;
        }

        // Get fixed container dimensions (same as overlay since it expands)
        let (container_w, container_h) = if let Some(fixed) = imp.fixed.borrow().as_ref() {
            (fixed.width() as f64, fixed.height() as f64)
        } else {
            return;
        };

        if container_w <= 0.0 || container_h <= 0.0 {
            return;
        }

        let img_w = imp.image_width.get() as f64;
        let img_h = imp.image_height.get() as f64;
        if img_w <= 0.0 || img_h <= 0.0 {
            return;
        }

        let old_pan_x = imp.pan_x.get();
        let old_pan_y = imp.pan_y.get();

        // Calculate current picture position in container
        let old_scaled_w = img_w * old_scale;
        let old_scaled_h = img_h * old_scale;
        let old_pic_x = (container_w - old_scaled_w) / 2.0 + old_pan_x;
        let old_pic_y = (container_h - old_scaled_h) / 2.0 + old_pan_y;

        // True zoom-to-cursor behavior: keep the anchor point stable.
        let center_x = container_w * 0.5;
        let center_y = container_h * 0.5;
        let inside_image = pointer_x >= old_pic_x
            && pointer_x <= old_pic_x + old_scaled_w
            && pointer_y >= old_pic_y
            && pointer_y <= old_pic_y + old_scaled_h;

        // If cursor is on blank space, anchor from center to avoid edge snapping.
        let anchor_x = if inside_image { pointer_x } else { center_x };
        let anchor_y = if inside_image { pointer_y } else { center_y };

        // Image-space anchor; do not clamp so blank-space zoom stays geometrically stable.
        let img_x = (anchor_x - old_pic_x) / old_scale;
        let img_y = (anchor_y - old_pic_y) / old_scale;

        let target_x = anchor_x;
        let target_y = anchor_y;

        // Place the same image-space point at the blended target position.
        let new_scaled_w = img_w * new_scale;
        let new_scaled_h = img_h * new_scale;
        let new_pic_x = target_x - img_x * new_scale;
        let new_pic_y = target_y - img_y * new_scale;

        // Convert back to pan (pic_pos = base + pan, so pan = pic_pos - base)
        let new_base_x = (container_w - new_scaled_w) / 2.0;
        let new_base_y = (container_h - new_scaled_h) / 2.0;
        let new_pan_x = new_pic_x - new_base_x;
        let new_pan_y = new_pic_y - new_base_y;

        imp.pan_x.set(new_pan_x);
        imp.pan_y.set(new_pan_y);
        imp.user_interacted.set(true);
        self.set_scale_internal(new_scale, true);
    }

    /// Set the zoom scale directly
    pub fn set_scale(&self, scale: f64) {
        self.set_scale_internal(scale, true);
    }

    /// Fit the image to the window
    pub fn fit_to_window(&self) {
        let imp = self.imp();
        if imp.is_video.get() {
            self.update_video_layout();
            return;
        }

        // Get window dimensions from overlay
        if let Some(overlay) = imp.overlay.borrow().as_ref() {
            let window_w = overlay.width() as f64;
            let window_h = overlay.height() as f64;
            let img_w = imp.image_width.get() as f64;
            let img_h = imp.image_height.get() as f64;

            if img_w > 0.0 && img_h > 0.0 && window_w > 0.0 && window_h > 0.0 {
                // Account for controls bar height (~50px)
                let available_h = window_h - 60.0;

                let scale_w = window_w / img_w;
                let scale_h = available_h / img_h;
                let scale = scale_w.min(scale_h).min(1.0); // Don't upscale beyond 1:1

                imp.pan_x.set(0.0);
                imp.pan_y.set(0.0);
                self.set_scale_internal(scale, false);
            }
        }
    }

    /// Pan by a delta
    pub fn pan_by(&self, dx: f64, dy: f64) {
        let imp = self.imp();
        if imp.is_video.get() {
            return;
        }
        imp.pan_x.set(imp.pan_x.get() + dx);
        imp.pan_y.set(imp.pan_y.get() + dy);
        imp.user_interacted.set(true);
        self.update_transform();
    }

    /// Update the picture transform based on scale and pan
    fn update_transform(&self) {
        let imp = self.imp();
        if imp.is_video.get() {
            self.update_video_layout();
            return;
        }

        let fixed = imp.fixed.borrow();
        let picture = imp.picture.borrow();

        if let (Some(fixed), Some(picture)) = (fixed.as_ref(), picture.as_ref()) {
            let container_w = fixed.width() as f64;
            let container_h = fixed.height() as f64;

            if container_w <= 0.0 || container_h <= 0.0 {
                return;
            }

            let scale = imp.scale.get();
            let pan_x = imp.pan_x.get();
            let pan_y = imp.pan_y.get();

            // Calculate the scaled image size
            let img_w = imp.image_width.get() as f64;
            let img_h = imp.image_height.get() as f64;
            let scaled_w = img_w * scale;
            let scaled_h = img_h * scale;

            let req_w = scaled_w.round() as i32;
            let req_h = scaled_h.round() as i32;
            if req_w != imp.last_req_w.get() || req_h != imp.last_req_h.get() {
                picture.set_size_request(req_w, req_h);
                imp.last_req_w.set(req_w);
                imp.last_req_h.set(req_h);
            }

            // Calculate position to center the image, then apply pan
            // Base position (centered in container)
            let base_x = (container_w - scaled_w) / 2.0;
            let base_y = (container_h - scaled_h) / 2.0;

            // Final position with pan
            let final_x = base_x + pan_x;
            let final_y = base_y + pan_y;

            let last_x = imp.last_pos_x.get();
            let last_y = imp.last_pos_y.get();
            if last_x.is_nan()
                || last_y.is_nan()
                || (final_x - last_x).abs() > 0.01
                || (final_y - last_y).abs() > 0.01
            {
                fixed.move_(picture, final_x, final_y);
                imp.last_pos_x.set(final_x);
                imp.last_pos_y.set(final_y);
            }
        }
    }

    /// Reset zoom and pan to default
    pub fn reset_view(&self) {
        let imp = self.imp();
        imp.scale.set(1.0);
        imp.pan_x.set(0.0);
        imp.pan_y.set(0.0);
        imp.scroll_accum.set(0.0);
        imp.last_req_w.set(-1);
        imp.last_req_h.set(-1);
        imp.last_pos_x.set(f64::NAN);
        imp.last_pos_y.set(f64::NAN);
        self.update_transform();
    }

    fn set_scale_internal(&self, scale: f64, user_interacted: bool) {
        let imp = self.imp();
        let clamped = scale.clamp(MIN_SCALE, MAX_SCALE);
        imp.scale.set(clamped);
        if user_interacted {
            imp.user_interacted.set(true);
        }
        self.update_transform();
        if !imp.is_video.get() {
            self.update_info_label(
                Some(imp.image_width.get()),
                Some(imp.image_height.get()),
                imp.is_loading.get(),
            );
        }
    }
}

impl Default for MediaViewer {
    fn default() -> Self {
        Self::new()
    }
}

// Free functions for image decoding (can be called from any thread)

fn format_timestamp(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as i64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{:02}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

/// Decode an image at downscaled resolution for fast preview
fn decode_image_downscaled(path: &Path, max_size: u32) -> Option<(Vec<u8>, u32, u32, u32, u32)> {
    let img = crate::image_loader::open_image(path).ok()?;
    let (orig_w, orig_h) = img.dimensions();

    // Calculate scale to fit within max_size
    let scale = if orig_w > orig_h {
        max_size as f32 / orig_w as f32
    } else {
        max_size as f32 / orig_h as f32
    };

    let (new_w, new_h) = if scale < 1.0 {
        (
            (orig_w as f32 * scale) as u32,
            (orig_h as f32 * scale) as u32,
        )
    } else {
        (orig_w, orig_h)
    };
    let new_w = new_w.max(1);
    let new_h = new_h.max(1);

    // Use a smooth filter and a slight blur to make preview intentionally soft.
    let resized = img
        .resize_exact(new_w, new_h, image::imageops::FilterType::Triangle)
        .blur(1.2);
    let (out_w, out_h) = resized.dimensions();
    let rgba = resized.to_rgba8();

    Some((rgba.into_raw(), out_w.max(1), out_h.max(1), orig_w, orig_h))
}

/// Decode an image at full resolution
fn decode_image_full(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let img = crate::image_loader::open_image(path).ok()?;
    let (width, height) = img.dimensions();
    let rgba = img.to_rgba8();

    Some((rgba.into_raw(), width.max(1), height.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scale_clamping() {
        // Test that scale values are properly clamped
        let clamped = 15.0_f64.clamp(MIN_SCALE, MAX_SCALE);
        assert_eq!(clamped, MAX_SCALE);

        let clamped = 0.01_f64.clamp(MIN_SCALE, MAX_SCALE);
        assert_eq!(clamped, MIN_SCALE);

        let clamped = 1.0_f64.clamp(MIN_SCALE, MAX_SCALE);
        assert_eq!(clamped, 1.0);
    }
}
