// Keybindings for idxd media browser
// Handles grid navigation, viewer controls, and keyboard shortcuts
//
// Keybindings:
// - Arrow keys / hjkl: Navigate grid
// - Enter: Open viewer for selected item
// - Escape: Close viewer, return to grid
// - Space: Play/pause (video) or toggle UI visibility
// - f: Toggle fullscreen
// - o: Open directory
// - r: Toggle recursive scan
// - s: Toggle shuffle
// - +: Toggle favorite
// - Delete: Delete file

use gdk4::Key;
use gtk4::prelude::*;
use gtk4::{EventControllerKey, PropagationPhase, Widget};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

/// Navigation direction for grid movement
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// Current view mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Grid,
    Viewer,
}

/// Selection state for the grid
pub struct GridSelection {
    /// Current row index
    pub row: u32,
    /// Current column index within the row
    pub col: u32,
    /// Number of rows in the grid
    pub row_count: u32,
    /// Items per row (can vary per row)
    pub items_per_row: Rc<dyn Fn(u32) -> u32>,
}

impl std::fmt::Debug for GridSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GridSelection")
            .field("row", &self.row)
            .field("col", &self.col)
            .field("row_count", &self.row_count)
            .field("items_per_row", &"<closure>")
            .finish()
    }
}

impl GridSelection {
    pub fn new<F>(items_per_row: F) -> Self
    where
        F: Fn(u32) -> u32 + 'static,
    {
        Self {
            row: 0,
            col: 0,
            row_count: 0,
            items_per_row: Rc::new(items_per_row),
        }
    }

    /// Move selection in the given direction
    pub fn move_selection(&mut self, direction: Direction) -> bool {
        if self.row_count == 0 {
            return false;
        }

        let old_row = self.row;
        let old_col = self.col;

        match direction {
            Direction::Up => {
                if self.row > 0 {
                    self.row -= 1;
                    // Clamp column to new row's item count
                    let new_row_items = (self.items_per_row)(self.row);
                    if new_row_items > 0 {
                        self.col = self.col.min(new_row_items - 1);
                    }
                }
            }
            Direction::Down => {
                if self.row < self.row_count.saturating_sub(1) {
                    self.row += 1;
                    // Clamp column to new row's item count
                    let new_row_items = (self.items_per_row)(self.row);
                    if new_row_items > 0 {
                        self.col = self.col.min(new_row_items - 1);
                    }
                }
            }
            Direction::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    // Wrap to previous row's last item
                    self.row -= 1;
                    let prev_row_items = (self.items_per_row)(self.row);
                    self.col = prev_row_items.saturating_sub(1);
                }
            }
            Direction::Right => {
                let current_row_items = (self.items_per_row)(self.row);
                if self.col < current_row_items.saturating_sub(1) {
                    self.col += 1;
                } else if self.row < self.row_count.saturating_sub(1) {
                    // Wrap to next row's first item
                    self.row += 1;
                    self.col = 0;
                }
            }
        }

        old_row != self.row || old_col != self.col
    }

    /// Get the current selection as (row, col)
    pub fn position(&self) -> (u32, u32) {
        (self.row, self.col)
    }

    /// Set the row count
    pub fn set_row_count(&mut self, count: u32) {
        self.row_count = count;
        // Clamp selection if needed
        if self.row >= count && count > 0 {
            self.row = count - 1;
        }
        if count > 0 {
            let items = (self.items_per_row)(self.row);
            if self.col >= items && items > 0 {
                self.col = items - 1;
            }
        }
    }
}

/// Callback type for selection changes
pub type SelectionChangedCallback = Box<dyn Fn(u32, u32)>;

/// Callback type for viewer open requests
pub type OpenViewerCallback = Box<dyn Fn(u32, u32, PathBuf)>;

/// Callback type for viewer close requests
pub type CloseViewerCallback = Box<dyn Fn()>;

/// Callback type for play/pause toggle
pub type PlayPauseCallback = Box<dyn Fn()>;

/// Callback type for UI visibility toggle
pub type ToggleUiCallback = Box<dyn Fn()>;

/// Callback type for fullscreen toggle
pub type ToggleFullscreenCallback = Box<dyn Fn()>;

/// Callback type for opening a directory prompt
pub type OpenDirectoryCallback = Box<dyn Fn()>;
/// Callback type for toggling recursive scan
pub type ToggleRecursiveCallback = Box<dyn Fn()>;
/// Callback type for toggling shuffle
pub type ToggleShuffleCallback = Box<dyn Fn()>;
/// Callback type for toggling favorite
pub type ToggleFavoriteCallback = Box<dyn Fn()>;
/// Callback type for deleting selected file
pub type DeleteSelectedCallback = Box<dyn Fn()>;

/// Keybinding manager for the media browser
pub struct Keybindings {
    controller: EventControllerKey,
    view_mode: Rc<Cell<ViewMode>>,
    selection: Rc<RefCell<GridSelection>>,
    // Callbacks
    on_selection_changed: Rc<RefCell<Option<SelectionChangedCallback>>>,
    on_open_viewer: Rc<RefCell<Option<OpenViewerCallback>>>,
    on_close_viewer: Rc<RefCell<Option<CloseViewerCallback>>>,
    on_play_pause: Rc<RefCell<Option<PlayPauseCallback>>>,
    on_toggle_ui: Rc<RefCell<Option<ToggleUiCallback>>>,
    on_toggle_fullscreen: Rc<RefCell<Option<ToggleFullscreenCallback>>>,
    on_open_directory: Rc<RefCell<Option<OpenDirectoryCallback>>>,
    on_toggle_recursive: Rc<RefCell<Option<ToggleRecursiveCallback>>>,
    on_toggle_shuffle: Rc<RefCell<Option<ToggleShuffleCallback>>>,
    on_toggle_favorite: Rc<RefCell<Option<ToggleFavoriteCallback>>>,
    on_delete_selected: Rc<RefCell<Option<DeleteSelectedCallback>>>,
    // Path lookup function
    get_path: Rc<RefCell<Option<Box<dyn Fn(u32, u32) -> Option<PathBuf>>>>>,
}

impl Keybindings {
    /// Create a new keybinding manager
    pub fn new() -> Self {
        let controller = EventControllerKey::new();
        controller.set_propagation_phase(PropagationPhase::Capture);

        let view_mode = Rc::new(Cell::new(ViewMode::Grid));
        let selection = Rc::new(RefCell::new(GridSelection::new(|_| 4))); // Default 4 items per row

        let on_selection_changed: Rc<RefCell<Option<SelectionChangedCallback>>> =
            Rc::new(RefCell::new(None));
        let on_open_viewer: Rc<RefCell<Option<OpenViewerCallback>>> = Rc::new(RefCell::new(None));
        let on_close_viewer: Rc<RefCell<Option<CloseViewerCallback>>> = Rc::new(RefCell::new(None));
        let on_play_pause: Rc<RefCell<Option<PlayPauseCallback>>> = Rc::new(RefCell::new(None));
        let on_toggle_ui: Rc<RefCell<Option<ToggleUiCallback>>> = Rc::new(RefCell::new(None));
        let on_toggle_fullscreen: Rc<RefCell<Option<ToggleFullscreenCallback>>> =
            Rc::new(RefCell::new(None));
        let on_open_directory: Rc<RefCell<Option<OpenDirectoryCallback>>> =
            Rc::new(RefCell::new(None));
        let on_toggle_recursive: Rc<RefCell<Option<ToggleRecursiveCallback>>> =
            Rc::new(RefCell::new(None));
        let on_toggle_shuffle: Rc<RefCell<Option<ToggleShuffleCallback>>> =
            Rc::new(RefCell::new(None));
        let on_toggle_favorite: Rc<RefCell<Option<ToggleFavoriteCallback>>> =
            Rc::new(RefCell::new(None));
        let on_delete_selected: Rc<RefCell<Option<DeleteSelectedCallback>>> =
            Rc::new(RefCell::new(None));
        let get_path: Rc<RefCell<Option<Box<dyn Fn(u32, u32) -> Option<PathBuf>>>>> =
            Rc::new(RefCell::new(None));

        // Clone references for the closure
        let view_mode_clone = view_mode.clone();
        let selection_clone = selection.clone();
        let on_selection_changed_clone = on_selection_changed.clone();
        let on_open_viewer_clone = on_open_viewer.clone();
        let on_close_viewer_clone = on_close_viewer.clone();
        let on_play_pause_clone = on_play_pause.clone();
        let on_toggle_ui_clone = on_toggle_ui.clone();
        let on_toggle_fullscreen_clone = on_toggle_fullscreen.clone();
        let on_open_directory_clone = on_open_directory.clone();
        let on_toggle_recursive_clone = on_toggle_recursive.clone();
        let on_toggle_shuffle_clone = on_toggle_shuffle.clone();
        let on_toggle_favorite_clone = on_toggle_favorite.clone();
        let on_delete_selected_clone = on_delete_selected.clone();
        let get_path_clone = get_path.clone();

        controller.connect_key_pressed(move |_controller, keyval, _keycode, _state| {
            let handled = Self::handle_key_press(
                keyval,
                &view_mode_clone,
                &selection_clone,
                &on_selection_changed_clone,
                &on_open_viewer_clone,
                &on_close_viewer_clone,
                &on_play_pause_clone,
                &on_toggle_ui_clone,
                &on_toggle_fullscreen_clone,
                &on_open_directory_clone,
                &on_toggle_recursive_clone,
                &on_toggle_shuffle_clone,
                &on_toggle_favorite_clone,
                &on_delete_selected_clone,
                &get_path_clone,
            );

            if handled {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });

        Self {
            controller,
            view_mode,
            selection,
            on_selection_changed,
            on_open_viewer,
            on_close_viewer,
            on_play_pause,
            on_toggle_ui,
            on_toggle_fullscreen,
            on_open_directory,
            on_toggle_recursive,
            on_toggle_shuffle,
            on_toggle_favorite,
            on_delete_selected,
            get_path,
        }
    }

    /// Attach keybindings to a widget (typically the main window)
    pub fn attach(&self, widget: &impl IsA<Widget>) {
        widget.add_controller(self.controller.clone());
    }

    /// Set the items-per-row lookup function
    pub fn set_items_per_row<F>(&self, f: F)
    where
        F: Fn(u32) -> u32 + 'static,
    {
        let mut selection = self.selection.borrow_mut();
        selection.items_per_row = Rc::new(f);
    }

    /// Set the path lookup function
    pub fn set_path_lookup<F>(&self, f: F)
    where
        F: Fn(u32, u32) -> Option<PathBuf> + 'static,
    {
        *self.get_path.borrow_mut() = Some(Box::new(f));
    }

    /// Set the row count
    pub fn set_row_count(&self, count: u32) {
        self.selection.borrow_mut().set_row_count(count);
    }

    /// Get the current selection position
    pub fn selection(&self) -> (u32, u32) {
        self.selection.borrow().position()
    }

    /// Set selection directly
    pub fn set_selection(&self, row: u32, col: u32) {
        let mut selection = self.selection.borrow_mut();
        if selection.row_count == 0 {
            return;
        }

        let row = row.min(selection.row_count.saturating_sub(1));
        let items = (selection.items_per_row)(row);
        let col = if items > 0 { col.min(items - 1) } else { 0 };

        selection.row = row;
        selection.col = col;
        drop(selection);

        if let Some(ref callback) = *self.on_selection_changed.borrow() {
            callback(row, col);
        }
    }

    /// Set the view mode
    pub fn set_view_mode(&self, mode: ViewMode) {
        self.view_mode.set(mode);
    }

    /// Connect callback for selection changes
    pub fn connect_selection_changed<F>(&self, callback: F)
    where
        F: Fn(u32, u32) + 'static,
    {
        *self.on_selection_changed.borrow_mut() = Some(Box::new(callback));
    }

    /// Connect callback for opening the viewer
    pub fn connect_open_viewer<F>(&self, callback: F)
    where
        F: Fn(u32, u32, PathBuf) + 'static,
    {
        *self.on_open_viewer.borrow_mut() = Some(Box::new(callback));
    }

    /// Connect callback for closing the viewer
    pub fn connect_close_viewer<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        *self.on_close_viewer.borrow_mut() = Some(Box::new(callback));
    }

    /// Connect callback for play/pause toggle
    pub fn connect_play_pause<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        *self.on_play_pause.borrow_mut() = Some(Box::new(callback));
    }

    /// Connect callback for UI visibility toggle
    pub fn connect_toggle_ui<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        *self.on_toggle_ui.borrow_mut() = Some(Box::new(callback));
    }

    /// Connect callback for fullscreen toggle
    pub fn connect_toggle_fullscreen<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        *self.on_toggle_fullscreen.borrow_mut() = Some(Box::new(callback));
    }

    /// Connect callback for open directory prompt
    pub fn connect_open_directory<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        *self.on_open_directory.borrow_mut() = Some(Box::new(callback));
    }

    /// Connect callback for toggling recursive scan
    pub fn connect_toggle_recursive<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        *self.on_toggle_recursive.borrow_mut() = Some(Box::new(callback));
    }

    /// Connect callback for toggling shuffle
    pub fn connect_toggle_shuffle<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        *self.on_toggle_shuffle.borrow_mut() = Some(Box::new(callback));
    }

    /// Connect callback for toggling favorite
    pub fn connect_toggle_favorite<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        *self.on_toggle_favorite.borrow_mut() = Some(Box::new(callback));
    }

    /// Connect callback for deleting selected file
    pub fn connect_delete_selected<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        *self.on_delete_selected.borrow_mut() = Some(Box::new(callback));
    }

    /// Handle a key press event
    #[allow(clippy::too_many_arguments)]
    fn handle_key_press(
        keyval: Key,
        view_mode: &Rc<Cell<ViewMode>>,
        selection: &Rc<RefCell<GridSelection>>,
        on_selection_changed: &Rc<RefCell<Option<SelectionChangedCallback>>>,
        on_open_viewer: &Rc<RefCell<Option<OpenViewerCallback>>>,
        on_close_viewer: &Rc<RefCell<Option<CloseViewerCallback>>>,
        on_play_pause: &Rc<RefCell<Option<PlayPauseCallback>>>,
        on_toggle_ui: &Rc<RefCell<Option<ToggleUiCallback>>>,
        on_toggle_fullscreen: &Rc<RefCell<Option<ToggleFullscreenCallback>>>,
        on_open_directory: &Rc<RefCell<Option<OpenDirectoryCallback>>>,
        on_toggle_recursive: &Rc<RefCell<Option<ToggleRecursiveCallback>>>,
        on_toggle_shuffle: &Rc<RefCell<Option<ToggleShuffleCallback>>>,
        on_toggle_favorite: &Rc<RefCell<Option<ToggleFavoriteCallback>>>,
        on_delete_selected: &Rc<RefCell<Option<DeleteSelectedCallback>>>,
        get_path: &Rc<RefCell<Option<Box<dyn Fn(u32, u32) -> Option<PathBuf>>>>>,
    ) -> bool {
        let mode = view_mode.get();

        // Handle Escape - close viewer
        if keyval == Key::Escape {
            if mode == ViewMode::Viewer {
                view_mode.set(ViewMode::Grid);
                if let Some(ref callback) = *on_close_viewer.borrow() {
                    callback();
                }
                return true;
            }
            return false;
        }

        // Handle Enter - open viewer
        if keyval == Key::Return || keyval == Key::KP_Enter {
            if mode == ViewMode::Grid {
                let sel = selection.borrow();
                let (row, col) = sel.position();

                // Get the path for the selected item
                if let Some(ref get_path_fn) = *get_path.borrow() {
                    if let Some(path) = get_path_fn(row, col) {
                        drop(sel); // Release borrow before callback
                        view_mode.set(ViewMode::Viewer);
                        if let Some(ref callback) = *on_open_viewer.borrow() {
                            callback(row, col, path);
                        }
                        return true;
                    }
                }
            }
            return false;
        }

        // Handle Space - play/pause or toggle UI
        if keyval == Key::space {
            if mode == ViewMode::Viewer {
                // In viewer: play/pause for video
                if let Some(ref callback) = *on_play_pause.borrow() {
                    callback();
                }
            } else {
                // In grid: toggle UI visibility
                if let Some(ref callback) = *on_toggle_ui.borrow() {
                    callback();
                }
            }
            return true;
        }

        // Handle fullscreen toggle
        if keyval == Key::f || keyval == Key::F {
            if let Some(ref callback) = *on_toggle_fullscreen.borrow() {
                callback();
            }
            return true;
        }

        // Handle open directory prompt
        if keyval == Key::o || keyval == Key::O {
            if let Some(ref callback) = *on_open_directory.borrow() {
                callback();
                return true;
            }
        }

        // Handle recursive toggle
        if keyval == Key::r || keyval == Key::R {
            if let Some(ref callback) = *on_toggle_recursive.borrow() {
                callback();
                return true;
            }
        }

        // Handle shuffle toggle
        if keyval == Key::s || keyval == Key::S {
            if let Some(ref callback) = *on_toggle_shuffle.borrow() {
                callback();
                return true;
            }
        }

        // Handle favorite toggle
        if keyval == Key::plus || keyval == Key::equal || keyval == Key::KP_Add {
            if let Some(ref callback) = *on_toggle_favorite.borrow() {
                callback();
                return true;
            }
        }

        // Handle delete
        if keyval == Key::Delete {
            if let Some(ref callback) = *on_delete_selected.borrow() {
                callback();
                return true;
            }
        }

        // Handle navigation keys (only in grid mode)
        if mode == ViewMode::Grid {
            let direction = match keyval {
                // Arrow keys
                Key::Up => Some(Direction::Up),
                Key::Down => Some(Direction::Down),
                Key::Left => Some(Direction::Left),
                Key::Right => Some(Direction::Right),
                // Vim-style keys (hjkl)
                Key::h => Some(Direction::Left),
                Key::j => Some(Direction::Down),
                Key::k => Some(Direction::Up),
                Key::l => Some(Direction::Right),
                _ => None,
            };

            if let Some(dir) = direction {
                let mut sel = selection.borrow_mut();
                if sel.move_selection(dir) {
                    let (row, col) = sel.position();
                    drop(sel); // Release borrow before callback
                    if let Some(ref callback) = *on_selection_changed.borrow() {
                        callback(row, col);
                    }
                }
                return true;
            }
        }

        // Handle viewer navigation (left/right for prev/next)
        if mode == ViewMode::Viewer {
            let direction = match keyval {
                Key::Left | Key::h => Some(Direction::Left),
                Key::Right | Key::l => Some(Direction::Right),
                _ => None,
            };

            if let Some(dir) = direction {
                let mut sel = selection.borrow_mut();
                if sel.move_selection(dir) {
                    let (row, col) = sel.position();

                    // Get the path for the new selection
                    if let Some(ref get_path_fn) = *get_path.borrow() {
                        if let Some(path) = get_path_fn(row, col) {
                            drop(sel); // Release borrow before callback
                            if let Some(ref callback) = *on_open_viewer.borrow() {
                                callback(row, col, path);
                            }
                            return true;
                        }
                    }
                }
                return true;
            }
        }

        false
    }
}

impl Default for Keybindings {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grid_selection_movement() {
        let mut selection = GridSelection::new(|_| 4); // 4 items per row
        selection.set_row_count(5);

        // Start at (0, 0)
        assert_eq!(selection.position(), (0, 0));

        // Move right
        assert!(selection.move_selection(Direction::Right));
        assert_eq!(selection.position(), (0, 1));

        // Move down
        assert!(selection.move_selection(Direction::Down));
        assert_eq!(selection.position(), (1, 1));

        // Move left
        assert!(selection.move_selection(Direction::Left));
        assert_eq!(selection.position(), (1, 0));

        // Move up
        assert!(selection.move_selection(Direction::Up));
        assert_eq!(selection.position(), (0, 0));
    }

    #[test]
    fn test_grid_selection_wrapping() {
        let mut selection = GridSelection::new(|_| 3); // 3 items per row
        selection.set_row_count(3);

        // Move to end of row
        selection.move_selection(Direction::Right);
        selection.move_selection(Direction::Right);
        assert_eq!(selection.position(), (0, 2));

        // Wrap to next row
        selection.move_selection(Direction::Right);
        assert_eq!(selection.position(), (1, 0));

        // Wrap back to previous row
        selection.move_selection(Direction::Left);
        assert_eq!(selection.position(), (0, 2));
    }

    #[test]
    fn test_grid_selection_boundaries() {
        let mut selection = GridSelection::new(|_| 4);
        selection.set_row_count(2);

        // Try to move up at top
        selection.move_selection(Direction::Up);
        assert_eq!(selection.position(), (0, 0));

        // Move to bottom right
        selection.row = 1;
        selection.col = 3;

        // Try to move down at bottom
        selection.move_selection(Direction::Down);
        assert_eq!(selection.position(), (1, 3));

        // Try to move right at end
        selection.move_selection(Direction::Right);
        assert_eq!(selection.position(), (1, 3));
    }

    #[test]
    fn test_variable_items_per_row() {
        // Simulate rows with different item counts
        let mut selection = GridSelection::new(|row| match row {
            0 => 5,
            1 => 3,
            2 => 4,
            _ => 0,
        });
        selection.set_row_count(3);

        // Start at row 0, col 4 (last item in row 0)
        selection.row = 0;
        selection.col = 4;

        // Move down - should clamp to col 2 (last item in row 1)
        selection.move_selection(Direction::Down);
        assert_eq!(selection.position(), (1, 2));

        // Move down again - col should stay 2
        selection.move_selection(Direction::Down);
        assert_eq!(selection.position(), (2, 2));
    }
}
