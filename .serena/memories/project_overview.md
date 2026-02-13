# idxd project overview
- Purpose: Local Linux desktop media browser to quickly scan large image/video folders and open focused viewer mode.
- Language/stack: Rust 2021, GTK4 UI, async via tokio, SQLite cache via rusqlite, image decoding via image crate, video via libmpv2.
- Entry behavior: GTK app (`lt.gtw.idxd`) that opens a window on activate/open and optionally takes a filesystem path argument.
- Primary goals in spec: responsive scrolling, justified-row layout, thumbnail + metadata caching, fast subsequent runs.
- Platform: Linux desktop, also packaged as Flatpak.
