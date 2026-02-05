// ListView setup for displaying media rows
// Uses GTK4 ListView with virtualization for smooth scrolling

use glib::Object;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{
    gio, glib, ListItem, ListView, NoSelection, PolicyType, ScrolledWindow, SignalListItemFactory,
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
    on_item_activated: Rc<RefCell<Option<Box<dyn Fn(u32, u32, PathBuf)>>>>,
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

        factory.connect_setup(move |_factory, list_item| {
            let list_item = list_item
                .downcast_ref::<ListItem>()
                .expect("ListItem expected");
            let row_widget = RowWidget::new();
            let on_item_activated = on_item_activated_setup.clone();
            row_widget.connect_item_activated(move |row, col, path| {
                if let Some(ref callback) = *on_item_activated.borrow() {
                    callback(row, col, path);
                }
            });
            list_item.set_child(Some(&row_widget));
        });

        // Bind: update the widget when data is bound to it
        factory.connect_bind(|_factory, list_item| {
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
            .hscrollbar_policy(PolicyType::Never)
            .vscrollbar_policy(PolicyType::Automatic)
            .kinetic_scrolling(true)
            .propagate_natural_width(false)
            .propagate_natural_height(false)
            .child(&list_view)
            .build();
        scrolled_window.set_min_content_width(0);
        scrolled_window.set_min_content_height(0);

        let visible_range = Rc::new(RefCell::new((0u32, 0u32)));

        Self {
            scrolled_window,
            list_view,
            model,
            visible_range,
            on_item_activated,
        }
    }

    /// Get the scrolled window widget to add to the window
    pub fn widget(&self) -> &ScrolledWindow {
        &self.scrolled_window
    }

    /// Get the content width available to the list view (excludes scrollbars).
    pub fn content_width(&self) -> f32 {
        let list_alloc = self.list_view.allocation().width() as f32;
        let scrolled_alloc = self.scrolled_window.allocation().width() as f32;
        let mut width = if scrolled_alloc > 0.0 {
            scrolled_alloc
        } else {
            list_alloc
        };
        if width <= 0.0 {
            return 0.0;
        }

        let vscrollbar = self.scrolled_window.vscrollbar();
        if vscrollbar.is_visible() {
            let vscrollbar_width = vscrollbar.allocated_width() as f32;
            width = (width - vscrollbar_width).max(0.0);
        }

        width
    }

    /// Debug helper for tracking allocation and scrollbar changes.
    pub fn debug_allocations(&self) -> (i32, i32, i32, bool) {
        let list_alloc = self.list_view.allocation().width();
        let scrolled_alloc = self.scrolled_window.allocation().width();
        let vscrollbar = self.scrolled_window.vscrollbar();
        let vscrollbar_width = vscrollbar.allocated_width();
        let vscrollbar_visible = vscrollbar.is_visible();
        (list_alloc, scrolled_alloc, vscrollbar_width, vscrollbar_visible)
    }

    /// Get the underlying model
    pub fn model(&self) -> &gio::ListStore {
        &self.model
    }

    /// Replace all rows
    pub fn set_rows(&self, rows: Vec<RowModel>) {
        let objects: Vec<RowModelObject> = rows.into_iter().map(RowModelObject::new).collect();
        self.model.splice(0, self.model.n_items(), &objects);
    }

    /// Get the number of rows
    pub fn row_count(&self) -> u32 {
        self.model.n_items()
    }

    /// Scroll to a specific row
    pub fn scroll_to_row(&self, index: u32) {
        // ListView doesn't have built-in scroll_to, use adjustment
        // This is a simplified version - real implementation would calculate
        // the exact position based on row heights
        let vadj = self.scrolled_window.vadjustment();
        // Estimate position (assuming average row height of 200px)
        let estimated_pos = index as f64 * 200.0;
        vadj.set_value(estimated_pos.min(vadj.upper() - vadj.page_size()));
    }

    /// Set up a callback for when visible range changes
    pub fn connect_visible_range_changed<F>(&self, callback: F)
    where
        F: Fn(u32, u32) + 'static,
    {
        let visible_range = self.visible_range.clone();
        let model = self.model.clone();

        let vadj = self.scrolled_window.vadjustment();
        vadj.connect_value_changed(move |adj| {
            let value = adj.value();
            let page_size = adj.page_size();
            let upper = adj.upper();

            // Estimate row height (could be made more accurate with actual heights)
            let avg_row_height = if model.n_items() > 0 && upper > 0.0 {
                upper / model.n_items() as f64
            } else {
                200.0
            };

            let first_visible = (value / avg_row_height).floor() as u32;
            let last_visible = ((value + page_size) / avg_row_height).ceil() as u32;
            let last_visible = last_visible.min(model.n_items().saturating_sub(1));

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
