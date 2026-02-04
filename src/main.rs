mod app;
mod layout;
mod models;
mod ui;

// These modules are complete implementations not yet wired into the main app
#[cfg(test)]
mod scanner;
#[cfg(test)]
mod thumbnails;

use app::IdxdApp;

fn main() {
    // Prefer C numeric locale up-front; GTK may later adjust locale again.
    std::env::set_var("LC_NUMERIC", "C");
    unsafe {
        libc::setlocale(libc::LC_NUMERIC, b"C\0".as_ptr().cast());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("idxd=info".parse().unwrap()),
        )
        .init();

    let app = IdxdApp::new();
    std::process::exit(app.run());
}
