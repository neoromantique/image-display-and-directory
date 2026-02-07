use gtk4::prelude::*;
use gtk4::{gio, Application};
use std::cell::RefCell;
use std::rc::Rc;

use crate::ui::MainWindow;

const APP_ID: &str = "lt.gtw.idxd";

thread_local! {
    static WINDOWS: RefCell<Vec<Rc<MainWindow>>> = const { RefCell::new(Vec::new()) };
}

pub struct IdxdApp {
    app: Application,
}

impl IdxdApp {
    pub fn new() -> Self {
        let app = Application::builder()
            .application_id(APP_ID)
            .flags(gio::ApplicationFlags::HANDLES_OPEN)
            .build();

        app.connect_activate(|app| {
            Self::open_window(app, None);
        });
        app.connect_open(|app, files, _hint| {
            let path = files.first().and_then(|f| f.path());
            Self::open_window(app, path.as_deref());
        });

        Self { app }
    }

    pub fn run(&self) -> i32 {
        self.app.run().into()
    }

    fn open_window(app: &Application, initial_path: Option<&std::path::Path>) {
        let window = MainWindow::new(app, initial_path);
        let window_id = Rc::as_ptr(&window) as usize;
        window.connect_close_request(move || {
            WINDOWS.with(|windows| {
                windows
                    .borrow_mut()
                    .retain(|existing| Rc::as_ptr(existing) as usize != window_id);
            });
        });
        window.present();
        WINDOWS.with(|windows| windows.borrow_mut().push(window));
    }
}

impl Default for IdxdApp {
    fn default() -> Self {
        Self::new()
    }
}
