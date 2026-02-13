#![allow(dead_code)]

mod app;
mod bench;
mod image_loader;
mod layout;
mod models;
mod scanner;
mod thumbnails;
mod ui;

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

    match bench::maybe_parse_args() {
        Ok(Some(args)) => match bench::run_benchmark(args) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("Benchmark failed: {e:#}");
                std::process::exit(1);
            }
        },
        Ok(None) => {}
        Err(e) => {
            eprintln!("Invalid benchmark arguments: {e:#}");
            std::process::exit(2);
        }
    }

    let app = IdxdApp::new();
    std::process::exit(app.run());
}
