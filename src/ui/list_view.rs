// ListView setup for displaying media rows
// Uses GTK4 ListView with virtualization for smooth scrolling

use glib::Object;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{
    gdk, gio, glib, ListItem, ListView, NoSelection, PolicyType, ScrolledWindow,
    SignalListItemFactory, Widget,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use super::row_widget::RowWidget;
use crate::models::RowModel;

// GObject wrapper for RowModel to use in ListStore
mod row_model_object {
    use super::*;

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct RowModelObjectInner {
            pub row_model: RefCell<Option<RowModel>>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for RowModelObjectInner {
            const NAME: &'static str = "IdxdRowModelObject";
            type Type = super::RowModelObject;
            type ParentType = glib::Object;
        }

        impl ObjectImpl for RowModelObjectInner {}
    }

    glib::wrapper! {
        pub struct RowModelObject(ObjectSubclass<imp::RowModelObjectInner>);
    }

    impl RowModelObject {
        pub fn new(row_model: RowModel) -> Self {
            let obj: Self = Object::builder().build();
            obj.imp().row_model.replace(Some(row_model));
            obj
        }

        pub fn row_model(&self) -> Option<RowModel> {
            self.imp().row_model.borrow().clone()
        }

        pub fn set_row_model(&self, row_model: RowModel) {
            self.imp().row_model.replace(Some(row_model));
        }
    }

    impl Default for RowModelObject {
        fn default() -> Self {
            Object::builder().build()
        }
    }
}

pub use row_model_object::RowModelObject;

/// MediaListView wraps a GTK ListView with virtualization support
/// for displaying rows of media items
pub struct MediaListView {
    scrolled_window: ScrolledWindow,
    list_view: ListView,
    model: gio::ListStore,
    // Track visible range for thumbnail loading optimization
    visible_range: Rc<RefCell<(u32, u32)>>,
    selection: Rc<RefCell<(u32, u32)>>,
    row_widgets: Rc<RefCell<Vec<glib::WeakRef<RowWidget>>>>,
    row_offsets: Rc<RefCell<Vec<f64>>>,
    on_item_activated: Rc<RefCell<Option<Box<dyn Fn(u32, u32, PathBuf)>>>>,
    on_item_context_menu:
        Rc<RefCell<Option<Box<dyn Fn(u32, u32, PathBuf, Widget, gdk::Rectangle)>>>>,
}

impl MediaListView {
    pub fn new() -> Self {
        // Create the backing ListStore for RowModelObject
        let model = gio::ListStore::new::<RowModelObject>();

        // Create selection model (NoSelection for now - we handle selection differently)
        let selection_model = NoSelection::new(Some(model.clone()));

        // Create the factory for list items
        let factory = SignalListItemFactory::new();

        // Setup: create the widget when a list item is created
        let on_item_activated: Rc<RefCell<Option<Box<dyn Fn(u32, u32, PathBuf)>>>> =
            Rc::new(RefCell::new(None));
        let on_item_activated_setup = on_item_activated.clone();
        let on_item_context_menu: Rc<
            RefCell<Option<Box<dyn Fn(u32, u32, PathBuf, Widget, gdk::Rectangle)>>>,
        > = Rc::new(RefCell::new(None));
        let on_item_context_menu_setup = on_item_context_menu.clone();
        let row_widgets: Rc<RefCell<Vec<glib::WeakRef<RowWidget>>>> =
            Rc::new(RefCell::new(Vec::new()));
        let row_widgets_setup = row_widgets.clone();
        let selection: Rc<RefCell<(u32, u32)>> = Rc::new(RefCell::new((0, 0)));
        let selection_bind = selection.clone();

        factory.connect_setup(move |_factory, list_item| {
            let list_item = list_item
                .downcast_ref::<ListItem>()
                .expect("ListItem expected");
            let row_widget = RowWidget::new();
            row_widgets_setup.borrow_mut().push(row_widget.downgrade());
            let on_item_activated = on_item_activated_setup.clone();
            let on_item_context_menu = on_item_context_menu_setup.clone();
            row_widget.connect_item_activated(move |row, col, path| {
                if let Some(ref callback) = *on_item_activated.borrow() {
                    callback(row, col, path);
                }
            });
            row_widget.connect_item_context_menu(move |row, col, path, widget, rect| {
                if let Some(ref callback) = *on_item_context_menu.borrow() {
                    callback(row, col, path, widget, rect);
                }
            });
            list_item.set_child(Some(&row_widget));
        });

        // Bind: update the widget when data is bound to it
        factory.connect_bind(move |_factory, list_item| {
            let list_item = list_item
                .downcast_ref::<ListItem>()
                .expect("ListItem expected");

            let row_model_obj = list_item
                .item()
                .and_downcast::<RowModelObject>()
                .expect("RowModelObject expected");

            let row_widget = list_item
                .child()
                .and_downcast::<RowWidget>()
                .expect("RowWidget expected");

            if let Some(row_model) = row_model_obj.row_model() {
                row_widget.bind(&row_model);
            }
            let (row, col) = *selection_bind.borrow();
            row_widget.update_selection(row, col);
        });

        // Unbind: clean up when data is unbound
        factory.connect_unbind(|_factory, list_item| {
            let list_item = list_item
                .downcast_ref::<ListItem>()
                .expect("ListItem expected");

            if let Some(row_widget) = list_item.child().and_downcast::<RowWidget>() {
                row_widget.unbind();
            }
        });

        // Teardown: clean up when the widget is destroyed (optional)
        factory.connect_teardown(|_factory, list_item| {
            let list_item = list_item
                .downcast_ref::<ListItem>()
                .expect("ListItem expected");
            list_item.set_child(Option::<&gtk4::Widget>::None);
        });

        // Create the ListView with the factory
        let list_view = ListView::new(Some(selection_model), Some(factory));
        list_view.set_single_click_activate(false);
        list_view.set_enable_rubberband(false);
        list_view.add_css_class("media-list-view");
        list_view.set_halign(gtk4::Align::Fill);
        list_view.set_hexpand(true);
        list_view.set_vexpand(true);

        // Wrap in ScrolledWindow for scrolling
        let scrolled_window = ScrolledWindow::builder()
            .hscrollbar_policy(PolicyType::Automatic)
            .vscrollbar_policy(PolicyType::Automatic)
            .kinetic_scrolling(true)
            .propagate_natural_width(false)
            .propagate_natural_height(false)
            .child(&list_view)
            .build();
        scrolled_window.set_min_content_width(0);
        scrolled_window.set_min_content_height(0);

        let visible_range = Rc::new(RefCell::new((0u32, 0u32)));
        let row_offsets = Rc::new(RefCell::new(Vec::new()));

        Self {
            scrolled_window,
            list_view,
            model,
            visible_range,
            selection,
            row_widgets,
            row_offsets,
            on_item_activated,
            on_item_context_menu,
        }
    }

    /// Get the scrolled window widget to add to the window
    pub fn widget(&self) -> &ScrolledWindow {
        &self.scrolled_window
    }

    /// Get the content width available to the list view (excludes scrollbars).
    pub fn content_width(&self) -> f32 {
        let scrolled_alloc = self.scrolled_window.width() as f32;
        if scrolled_alloc <= 0.0 {
            return 0.0;
        }
        let mut width = scrolled_alloc;

        let vscrollbar = self.scrolled_window.vscrollbar();
        if vscrollbar.is_visible() {
            let vscrollbar_width = vscrollbar.width() as f32;
            if vscrollbar_width > 0.0 {
                width = (width - vscrollbar_width).max(0.0);
            }
        }

        width
    }

    /// Debug helper for tracking allocation and scrollbar changes.
    pub fn debug_allocations(&self) -> (i32, i32, i32, bool) {
        let list_alloc = self.list_view.width();
        let scrolled_alloc = self.scrolled_window.width();
        let vscrollbar = self.scrolled_window.vscrollbar();
        let vscrollbar_width = vscrollbar.width();
        let vscrollbar_visible = vscrollbar.is_visible();
        (
            list_alloc,
            scrolled_alloc,
            vscrollbar_width,
            vscrollbar_visible,
        )
    }

    /// Get the underlying model
    pub fn model(&self) -> &gio::ListStore {
        &self.model
    }

    /// Replace all rows
    pub fn set_rows(&self, rows: Vec<RowModel>) {
        let mut offsets = Vec::with_capacity(rows.len() + 1);
        offsets.push(0.0);
        let mut y = 0.0;
        for row in &rows {
            y += row.height_px as f64;
            offsets.push(y);
        }
        *self.row_offsets.borrow_mut() = offsets;

        let objects: Vec<RowModelObject> = rows.into_iter().map(RowModelObject::new).collect();
        self.model.splice(0, self.model.n_items(), &objects);
    }

    pub fn set_selection(&self, row: u32, col: u32) {
        *self.selection.borrow_mut() = (row, col);
        let mut widgets = self.row_widgets.borrow_mut();
        widgets.retain(|weak| {
            if let Some(widget) = weak.upgrade() {
                widget.update_selection(row, col);
                true
            } else {
                false
            }
        });
    }

    /// Get the number of rows
    pub fn row_count(&self) -> u32 {
        self.model.n_items()
    }

    /// Scroll to a specific row
    pub fn scroll_to_row(&self, index: u32) {
        let vadj = self.scrolled_window.vadjustment();
        let offsets = self.row_offsets.borrow();
        let position = offsets
            .get(index as usize)
            .copied()
            .unwrap_or_else(|| vadj.value());
        let max_pos = (vadj.upper() - vadj.page_size()).max(0.0);
        vadj.set_value(position.min(max_pos));
    }

    /// Set up a callback for when visible range changes
    pub fn connect_visible_range_changed<F>(&self, callback: F)
    where
        F: Fn(u32, u32) + 'static,
    {
        let visible_range = self.visible_range.clone();
        let model = self.model.clone();
        let row_offsets = self.row_offsets.clone();

        let vadj = self.scrolled_window.vadjustment();
        vadj.connect_value_changed(move |adj| {
            let value = adj.value();
            let page_size = adj.page_size();
            let count = model.n_items();
            if count == 0 {
                return;
            }
            let offsets = row_offsets.borrow();
            if offsets.len() < (count as usize + 1) {
                return;
            }
            let find_row = |y: f64| -> u32 {
                let idx = offsets.partition_point(|off| *off <= y);
                idx.saturating_sub(1).min(count as usize - 1) as u32
            };
            let first_visible = find_row(value);
            let last_visible = find_row(value + page_size);

            let mut range = visible_range.borrow_mut();
            if *range != (first_visible, last_visible) {
                *range = (first_visible, last_visible);
                drop(range);
                callback(first_visible, last_visible);
            }
        });
    }

    /// Notify when the vertical scrollbar visibility changes.
    pub fn connect_vscrollbar_visibility_changed<F>(&self, callback: F)
    where
        F: Fn(bool) + 'static,
    {
        let vscrollbar = self.scrolled_window.vscrollbar();
        vscrollbar.connect_notify_local(Some("visible"), move |scrollbar, _| {
            callback(scrollbar.is_visible());
        });
    }

    pub fn connect_item_activated<F>(&self, callback: F)
    where
        F: Fn(u32, u32, PathBuf) + 'static,
    {
        *self.on_item_activated.borrow_mut() = Some(Box::new(callback));
    }

    pub fn connect_item_context_menu<F>(&self, callback: F)
    where
        F: Fn(u32, u32, PathBuf, Widget, gdk::Rectangle) + 'static,
    {
        *self.on_item_context_menu.borrow_mut() = Some(Box::new(callback));
    }
}

impl Default for MediaListView {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_row_model_object() {
        // This test requires GTK initialization
        // gtk4::init().ok();
    }
}
