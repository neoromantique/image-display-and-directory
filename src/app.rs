use gtk4::prelude::*;
use gtk4::{gio, Application};

use crate::ui::MainWindow;

const APP_ID: &str = "com.idxd.MediaBrowser";

pub struct IdxdApp {
    app: Application,
}

impl IdxdApp {
    pub fn new() -> Self {
        let app = Application::builder()
            .application_id(APP_ID)
            .flags(gio::ApplicationFlags::HANDLES_OPEN)
            .build();

        app.connect_activate(Self::on_activate);
        app.connect_open(Self::on_open);

        Self { app }
    }

    pub fn run(&self) -> i32 {
        self.app.run().into()
    }

    fn on_activate(app: &Application) {
        let window = MainWindow::new(app, None);
        window.present();
        // Keep the window alive by storing it on the Application.
        unsafe {
            app.set_data("main-window", window);
        }
    }

    fn on_open(app: &Application, files: &[gio::File], _hint: &str) {
        let path = files.first().and_then(|f| f.path());
        let window = MainWindow::new(app, path.as_deref());
        window.present();
        // Keep the window alive by storing it on the Application.
        unsafe {
            app.set_data("main-window", window);
        }
    }
}

impl Default for IdxdApp {
    fn default() -> Self {
        Self::new()
    }
}
