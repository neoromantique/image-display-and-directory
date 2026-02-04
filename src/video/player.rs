//! Video player using libmpv embedded in GTK4
//!
//! This module provides video playback functionality for the idxd media browser.
//! It uses libmpv's OpenGL render API embedded in a GTK4 GLArea widget.

use glib::clone;
use gtk4::gdk;
use gtk4::prelude::*;
use gtk4::{glib, GLArea};
use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType};
use libmpv2::Mpv;
use once_cell::sync::OnceCell;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Callback type for playback state changes
pub type PlaybackStateCallback = Box<dyn Fn(PlaybackState) + 'static>;

/// Callback type for position updates (position, duration)
pub type PositionCallback = Box<dyn Fn(f64, f64) + 'static>;

/// Callback type for media end events
pub type MediaEndCallback = Box<dyn Fn() + 'static>;

/// Playback state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
}

/// Seek direction for relative seeks
#[derive(Debug, Clone, Copy)]
pub enum SeekDirection {
    Forward,
    Backward,
}

/// Ensure epoxy is initialized once
static EPOXY_INITIALIZED: OnceCell<()> = OnceCell::new();

fn ensure_epoxy_initialized() {
    EPOXY_INITIALIZED.get_or_init(|| {
        // Load epoxy symbols from the current process
        // GTK4 already links epoxy, so symbols should be available
        epoxy::load_with(|s| {
            // Try to find the symbol in the current process
            unsafe {
                let handle = libc::dlopen(std::ptr::null(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
                if handle.is_null() {
                    return std::ptr::null();
                }
                let c_str =
                    std::ffi::CString::new(s).expect("Failed to create CString for symbol lookup");
                let sym = libc::dlsym(handle, c_str.as_ptr());
                libc::dlclose(handle);
                sym
            }
        });
    });
}

/// GL context wrapper for OpenGL init params (unit type since we use epoxy)
struct GlContext;

/// Video player state - holds mpv instance
struct PlayerState {
    mpv: Option<Mpv>,
    render_ctx: Option<RenderContext>,
    current_path: Option<PathBuf>,
    pending_path: Option<PathBuf>,
    playback_state: PlaybackState,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            mpv: None,
            render_ctx: None,
            current_path: None,
            pending_path: None,
            playback_state: PlaybackState::Stopped,
        }
    }
}

/// Video player widget using libmpv
///
/// This player embeds mpv in a GTK4 GLArea widget using the OpenGL render API.
/// The mpv instance is kept alive across file selections to avoid re-initialization
/// overhead.
pub struct VideoPlayer {
    /// The GLArea widget for rendering
    gl_area: GLArea,
    /// Shared player state (mpv instance lives here)
    state: Rc<RefCell<PlayerState>>,
    /// Whether the player has been initialized
    initialized: Rc<Cell<bool>>,
    /// Callbacks for state changes
    state_callbacks: Rc<RefCell<Vec<PlaybackStateCallback>>>,
    /// Callbacks for position updates
    position_callbacks: Rc<RefCell<Vec<PositionCallback>>>,
    /// Callbacks for media end
    end_callbacks: Rc<RefCell<Vec<MediaEndCallback>>>,
    /// Timer source for position updates
    position_timer: Rc<RefCell<Option<glib::SourceId>>>,
    /// Flag to signal render thread needs update
    needs_render: Arc<AtomicBool>,
}

impl VideoPlayer {
    /// Create a new video player widget
    pub fn new() -> Self {
        let gl_area = GLArea::new();
        gl_area.set_auto_render(false);
        gl_area.set_has_depth_buffer(false);
        gl_area.set_has_stencil_buffer(false);
        gl_area.set_hexpand(true);
        gl_area.set_vexpand(true);

        // Use GLES if available for better compatibility
        gl_area.set_allowed_apis(gdk::GLAPI::GL | gdk::GLAPI::GLES);

        let state = Rc::new(RefCell::new(PlayerState::default()));
        let initialized = Rc::new(Cell::new(false));
        let state_callbacks: Rc<RefCell<Vec<PlaybackStateCallback>>> =
            Rc::new(RefCell::new(Vec::new()));
        let position_callbacks: Rc<RefCell<Vec<PositionCallback>>> =
            Rc::new(RefCell::new(Vec::new()));
        let end_callbacks: Rc<RefCell<Vec<MediaEndCallback>>> = Rc::new(RefCell::new(Vec::new()));
        let position_timer = Rc::new(RefCell::new(None));
        let needs_render = Arc::new(AtomicBool::new(false));

        let player = Self {
            gl_area,
            state,
            initialized,
            state_callbacks,
            position_callbacks,
            end_callbacks,
            position_timer,
            needs_render,
        };

        player.setup_gl_callbacks();
        player
    }

    /// Get the GTK widget for embedding in the UI
    pub fn widget(&self) -> &GLArea {
        &self.gl_area
    }

    /// Set up OpenGL callbacks on the GLArea
    fn setup_gl_callbacks(&self) {
        let state = self.state.clone();
        let initialized = self.initialized.clone();
        let needs_render = self.needs_render.clone();
        let state_callbacks = self.state_callbacks.clone();
        let position_callbacks = self.position_callbacks.clone();
        let end_callbacks = self.end_callbacks.clone();
        let position_timer = self.position_timer.clone();

        // Realize callback - initialize mpv when GL context is ready
        self.gl_area.connect_realize(clone!(
            #[strong]
            state,
            #[strong]
            initialized,
            #[strong]
            needs_render,
            #[strong]
            state_callbacks,
            #[strong]
            position_callbacks,
            #[strong]
            end_callbacks,
            #[strong]
            position_timer,
            move |gl_area| {
                gl_area.make_current();
                if let Some(err) = gl_area.error() {
                    tracing::error!("GLArea error on realize: {}", err);
                    return;
                }

                if initialized.get() {
                    return;
                }

                // Initialize epoxy for GL function pointer resolution
                ensure_epoxy_initialized();

                match Self::init_mpv(needs_render.clone()) {
                    Ok((mpv, render_ctx)) => {
                        let mut state_mut = state.borrow_mut();
                        state_mut.mpv = Some(mpv);
                        state_mut.render_ctx = Some(render_ctx);
                        let pending_path = state_mut.pending_path.take();
                        initialized.set(true);
                        tracing::info!("mpv initialized successfully");

                        if let Some(path) = pending_path {
                            if let Some(path_str) = path.to_str() {
                                if let Some(ref mpv) = state_mut.mpv {
                                    if let Err(e) = mpv.command("loadfile", &[path_str, "replace"])
                                    {
                                        tracing::error!(
                                            "Failed to play queued file after init: {}",
                                            e
                                        );
                                    } else {
                                        state_mut.current_path = Some(path);
                                        state_mut.playback_state = PlaybackState::Playing;
                                        drop(state_mut);

                                        for callback in state_callbacks.borrow().iter() {
                                            callback(PlaybackState::Playing);
                                        }

                                        Self::stop_position_timer_handle(&position_timer);
                                        Self::start_position_timer_handle(
                                            state.clone(),
                                            position_callbacks.clone(),
                                            end_callbacks.clone(),
                                            position_timer.clone(),
                                            gl_area.clone(),
                                        );
                                        gl_area.queue_render();
                                    }
                                }
                            } else {
                                tracing::error!("Queued path is not valid UTF-8: {:?}", path);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to initialize mpv: {}", e);
                    }
                }
            }
        ));

        // Unrealize callback - clean up mpv
        self.gl_area.connect_unrealize(clone!(
            #[strong]
            state,
            #[strong]
            initialized,
            move |gl_area| {
                gl_area.make_current();
                let mut state = state.borrow_mut();
                // Drop render context first, then mpv
                state.render_ctx = None;
                state.mpv = None;
                initialized.set(false);
                tracing::info!("mpv cleaned up");
            }
        ));

        // Render callback - draw the video frame
        self.gl_area.connect_render(clone!(
            #[strong]
            state,
            move |gl_area, _gl_context| {
                let state = state.borrow();
                if let Some(ref render_ctx) = state.render_ctx {
                    let scale = gl_area.scale_factor();
                    let width = gl_area.width() * scale;
                    let height = gl_area.height() * scale;

                    // Render to the default framebuffer (0)
                    // flip=true because GTK's coordinate system is flipped
                    if let Err(e) = render_ctx.render::<GlContext>(0, width, height, true) {
                        tracing::error!("mpv render error: {}", e);
                    }
                }
                glib::Propagation::Stop
            }
        ));

        // Resize callback - queue redraw when size changes
        self.gl_area
            .connect_resize(move |gl_area, _width, _height| {
                gl_area.queue_render();
            });
    }

    /// Initialize mpv with OpenGL rendering
    fn init_mpv(
        needs_render: Arc<AtomicBool>,
    ) -> Result<(Mpv, RenderContext), Box<dyn std::error::Error>> {
        // GTK initialization may reset locale after program start; enforce this
        // right before touching libmpv, which requires LC_NUMERIC=C.
        let locale_set = unsafe { libc::setlocale(libc::LC_NUMERIC, b"C\0".as_ptr().cast()) };
        if locale_set.is_null() {
            tracing::warn!("Failed to set LC_NUMERIC=C before mpv init");
        }

        // Create mpv with custom initialization
        let mut mpv = Mpv::with_initializer(|init| {
            // Enable hardware decoding with auto fallback
            init.set_option("hwdec", "auto-safe")?;

            // Video output configuration for OpenGL rendering
            init.set_option("vo", "libmpv")?;

            // Audio output - try pipewire first, then pulse, then alsa
            init.set_option("ao", "pipewire,pulse,alsa")?;

            // Keep the player open after video ends (allows seeking back)
            init.set_option("keep-open", "yes")?;

            // Don't start paused
            init.set_option("pause", "no")?;

            // Enable cache for smooth playback
            init.set_option("cache", "yes")?;

            // Demuxer cache size
            init.set_option("demuxer-max-bytes", "50MiB")?;

            // Disable OSD (we'll provide our own UI)
            init.set_option("osd-level", 0i64)?;

            // Terminal output disabled (we're embedded)
            init.set_option("terminal", false)?;

            // Input default bindings disabled (we handle input ourselves)
            init.set_option("input-default-bindings", false)?;

            // Log level
            init.set_option("msg-level", "all=warn")?;

            Ok(())
        })?;

        // Get proc address function for OpenGL using epoxy
        fn get_proc_address(_ctx: &GlContext, name: &str) -> *mut c_void {
            epoxy::get_proc_addr(name) as *mut c_void
        }

        // Set up OpenGL init params
        let gl_init_params = OpenGLInitParams {
            get_proc_address,
            ctx: GlContext,
        };

        // Create render context
        let render_params = vec![
            RenderParam::ApiType(RenderParamApiType::OpenGl),
            RenderParam::InitParams(gl_init_params),
        ];

        // Get the raw mpv handle for creating render context
        // SAFETY: We have exclusive access to mpv here during initialization
        let render_ctx =
            unsafe { RenderContext::new(mpv.ctx.as_mut(), render_params.into_iter())? };

        // Set up update callback to trigger redraws when new frames are ready
        // Note: This callback is called from mpv's thread, so we just set a flag
        // and let the main thread handle the actual render request
        // Note: set_update_callback requires &mut self, but we can't call it here
        // because render_ctx would need to be mutable. We'll handle frame updates
        // via polling in the position timer instead.
        let _ = needs_render; // Will be used for render update signaling

        Ok((mpv, render_ctx))
    }

    /// Load and play a video file
    pub fn play(&self, path: &Path) {
        if !self.initialized.get() {
            self.state.borrow_mut().pending_path = Some(path.to_path_buf());
            tracing::warn!(
                "Player not initialized yet; queued file for playback: {}",
                path.display()
            );
            return;
        }

        let path_str = match path.to_str() {
            Some(s) => s,
            None => {
                tracing::error!("Invalid path: {:?}", path);
                return;
            }
        };

        let mut state = self.state.borrow_mut();
        if let Some(ref mpv) = state.mpv {
            // Load the file
            if let Err(e) = mpv.command("loadfile", &[path_str, "replace"]) {
                tracing::error!("Failed to load file: {}", e);
                return;
            }

            state.current_path = Some(path.to_path_buf());
            state.playback_state = PlaybackState::Playing;

            tracing::info!("Playing: {}", path_str);
        }

        drop(state);

        // Notify state change
        self.notify_state_change(PlaybackState::Playing);

        // Start position update timer
        self.start_position_timer();

        // Queue a render
        self.gl_area.queue_render();
    }

    /// Toggle play/pause
    pub fn toggle_pause(&self) {
        if !self.initialized.get() {
            return;
        }

        let mut state = self.state.borrow_mut();
        if let Some(ref mpv) = state.mpv {
            let new_state = match state.playback_state {
                PlaybackState::Playing => {
                    let _ = mpv.set_property("pause", true);
                    PlaybackState::Paused
                }
                PlaybackState::Paused => {
                    let _ = mpv.set_property("pause", false);
                    PlaybackState::Playing
                }
                PlaybackState::Stopped => return,
            };
            state.playback_state = new_state;

            drop(state);
            self.notify_state_change(new_state);

            if new_state == PlaybackState::Playing {
                self.start_position_timer();
            } else {
                self.stop_position_timer();
            }
        }
    }

    /// Pause playback
    pub fn pause(&self) {
        if !self.initialized.get() {
            return;
        }

        let mut state = self.state.borrow_mut();
        if let Some(ref mpv) = state.mpv {
            if state.playback_state == PlaybackState::Playing {
                let _ = mpv.set_property("pause", true);
                state.playback_state = PlaybackState::Paused;
                drop(state);
                self.notify_state_change(PlaybackState::Paused);
                self.stop_position_timer();
            }
        }
    }

    /// Resume playback
    pub fn resume(&self) {
        if !self.initialized.get() {
            return;
        }

        let mut state = self.state.borrow_mut();
        if let Some(ref mpv) = state.mpv {
            if state.playback_state == PlaybackState::Paused {
                let _ = mpv.set_property("pause", false);
                state.playback_state = PlaybackState::Playing;
                drop(state);
                self.notify_state_change(PlaybackState::Playing);
                self.start_position_timer();
            }
        }
    }

    /// Stop playback
    pub fn stop(&self) {
        if !self.initialized.get() {
            return;
        }

        let mut state = self.state.borrow_mut();
        if let Some(ref mpv) = state.mpv {
            let _ = mpv.command("stop", &[]);
            state.playback_state = PlaybackState::Stopped;
            state.current_path = None;
        }

        drop(state);
        self.notify_state_change(PlaybackState::Stopped);
        self.stop_position_timer();
        self.gl_area.queue_render();
    }

    /// Seek to an absolute position in seconds
    pub fn seek_absolute(&self, position: f64) {
        if !self.initialized.get() {
            return;
        }

        let state = self.state.borrow();
        if let Some(ref mpv) = state.mpv {
            let pos_str = format!("{:.3}", position);
            let _ = mpv.command("seek", &[&pos_str, "absolute"]);
        }

        drop(state);
        self.gl_area.queue_render();
    }

    /// Seek relative to current position
    pub fn seek_relative(&self, seconds: f64, direction: SeekDirection) {
        if !self.initialized.get() {
            return;
        }

        let offset = match direction {
            SeekDirection::Forward => seconds,
            SeekDirection::Backward => -seconds,
        };

        let state = self.state.borrow();
        if let Some(ref mpv) = state.mpv {
            let offset_str = format!("{:.3}", offset);
            let _ = mpv.command("seek", &[&offset_str, "relative"]);
        }

        drop(state);
        self.gl_area.queue_render();
    }

    /// Seek forward by default amount (5 seconds)
    pub fn seek_forward(&self) {
        self.seek_relative(5.0, SeekDirection::Forward);
    }

    /// Seek backward by default amount (5 seconds)
    pub fn seek_backward(&self) {
        self.seek_relative(5.0, SeekDirection::Backward);
    }

    /// Get current playback position in seconds
    pub fn position(&self) -> f64 {
        let state = self.state.borrow();
        if let Some(ref mpv) = state.mpv {
            mpv.get_property("time-pos").unwrap_or(0.0)
        } else {
            0.0
        }
    }

    /// Get total duration in seconds
    pub fn duration(&self) -> f64 {
        let state = self.state.borrow();
        if let Some(ref mpv) = state.mpv {
            mpv.get_property("duration").unwrap_or(0.0)
        } else {
            0.0
        }
    }

    /// Get current playback state
    pub fn playback_state(&self) -> PlaybackState {
        self.state.borrow().playback_state
    }

    /// Check if currently playing
    pub fn is_playing(&self) -> bool {
        self.state.borrow().playback_state == PlaybackState::Playing
    }

    /// Check if paused
    pub fn is_paused(&self) -> bool {
        self.state.borrow().playback_state == PlaybackState::Paused
    }

    /// Get the currently playing file path
    pub fn current_path(&self) -> Option<PathBuf> {
        self.state.borrow().current_path.clone()
    }

    /// Set volume (0-100)
    pub fn set_volume(&self, volume: i64) {
        if !self.initialized.get() {
            return;
        }

        let volume = volume.clamp(0, 100);
        let state = self.state.borrow();
        if let Some(ref mpv) = state.mpv {
            let _ = mpv.set_property("volume", volume);
        }
    }

    /// Get volume (0-100)
    pub fn volume(&self) -> i64 {
        let state = self.state.borrow();
        if let Some(ref mpv) = state.mpv {
            mpv.get_property("volume").unwrap_or(100)
        } else {
            100
        }
    }

    /// Toggle mute
    pub fn toggle_mute(&self) {
        if !self.initialized.get() {
            return;
        }

        let state = self.state.borrow();
        if let Some(ref mpv) = state.mpv {
            let muted: bool = mpv.get_property("mute").unwrap_or(false);
            let _ = mpv.set_property("mute", !muted);
        }
    }

    /// Check if muted
    pub fn is_muted(&self) -> bool {
        let state = self.state.borrow();
        if let Some(ref mpv) = state.mpv {
            mpv.get_property("mute").unwrap_or(false)
        } else {
            false
        }
    }

    /// Register a callback for playback state changes
    pub fn connect_state_changed<F: Fn(PlaybackState) + 'static>(&self, callback: F) {
        self.state_callbacks.borrow_mut().push(Box::new(callback));
    }

    /// Register a callback for position updates
    pub fn connect_position_changed<F: Fn(f64, f64) + 'static>(&self, callback: F) {
        self.position_callbacks
            .borrow_mut()
            .push(Box::new(callback));
    }

    /// Register a callback for media end events
    pub fn connect_media_ended<F: Fn() + 'static>(&self, callback: F) {
        self.end_callbacks.borrow_mut().push(Box::new(callback));
    }

    /// Notify all state change callbacks
    fn notify_state_change(&self, state: PlaybackState) {
        for callback in self.state_callbacks.borrow().iter() {
            callback(state);
        }
    }

    /// Notify all end callbacks
    #[allow(dead_code)]
    fn notify_media_ended(&self) {
        for callback in self.end_callbacks.borrow().iter() {
            callback();
        }
    }

    /// Start the position update timer
    fn start_position_timer(&self) {
        Self::stop_position_timer_handle(&self.position_timer);
        Self::start_position_timer_handle(
            self.state.clone(),
            self.position_callbacks.clone(),
            self.end_callbacks.clone(),
            self.position_timer.clone(),
            self.gl_area.clone(),
        );
    }

    /// Stop the position update timer
    fn stop_position_timer(&self) {
        Self::stop_position_timer_handle(&self.position_timer);
    }

    fn start_position_timer_handle(
        state: Rc<RefCell<PlayerState>>,
        position_callbacks: Rc<RefCell<Vec<PositionCallback>>>,
        end_callbacks: Rc<RefCell<Vec<MediaEndCallback>>>,
        position_timer: Rc<RefCell<Option<glib::SourceId>>>,
        gl_area: GLArea,
    ) {
        // Update position every 100ms
        let source_id = glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            let state_ref = state.borrow();
            if let Some(ref mpv) = state_ref.mpv {
                let position: f64 = mpv.get_property("time-pos").unwrap_or(0.0);
                let duration: f64 = mpv.get_property("duration").unwrap_or(0.0);
                let eof: bool = mpv.get_property("eof-reached").unwrap_or(false);

                drop(state_ref);

                for callback in position_callbacks.borrow().iter() {
                    callback(position, duration);
                }

                if eof {
                    for callback in end_callbacks.borrow().iter() {
                        callback();
                    }
                }

                gl_area.queue_render();
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });

        *position_timer.borrow_mut() = Some(source_id);
    }

    fn stop_position_timer_handle(position_timer: &Rc<RefCell<Option<glib::SourceId>>>) {
        if let Some(source_id) = position_timer.borrow_mut().take() {
            source_id.remove();
        }
    }

    /// Process mpv events (call periodically from main loop if needed)
    pub fn process_events(&mut self) {
        let mut state = self.state.borrow_mut();
        if let Some(ref mut mpv) = state.mpv {
            // Process events with 0 timeout (non-blocking)
            let event_ctx = mpv.event_context_mut();
            while let Some(event) = event_ctx.wait_event(0.0) {
                match event {
                    Ok(libmpv2::events::Event::EndFile(_reason)) => {
                        tracing::debug!("End file event");
                    }
                    Ok(libmpv2::events::Event::FileLoaded) => {
                        tracing::debug!("File loaded");
                    }
                    Ok(libmpv2::events::Event::Seek) => {
                        tracing::debug!("Seek completed");
                    }
                    Ok(libmpv2::events::Event::PlaybackRestart) => {
                        tracing::debug!("Playback restart");
                    }
                    Err(e) => {
                        tracing::warn!("mpv event error: {}", e);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Check if the player needs a render update and queue it
    pub fn check_render_update(&self) {
        if self.needs_render.swap(false, Ordering::SeqCst) {
            self.gl_area.queue_render();
        }
    }
}

impl Default for VideoPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.stop_position_timer();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_playback_state() {
        assert_eq!(PlaybackState::Stopped, PlaybackState::Stopped);
        assert_ne!(PlaybackState::Playing, PlaybackState::Paused);
    }

    #[test]
    fn test_seek_direction() {
        match SeekDirection::Forward {
            SeekDirection::Forward => {}
            SeekDirection::Backward => panic!("Wrong direction"),
        }
    }
}
