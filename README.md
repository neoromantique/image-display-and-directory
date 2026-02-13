# idxd

`idxd` is a local desktop media browser for Linux.

<img src="assets/logo-v2.jpg" alt="idxd logo" width="128" />

## What It Does

- Displays folders and media files in a justified grid layout.
- Supports common image formats (`jpg`, `png`, `webp`, `gif`, `bmp`, `tiff`).
- Supports common video formats (`webm`, `mp4`, `mkv`, `avi`, `mov`).
- Opens a focused viewer mode for selected items.
- Provides keyboard-first navigation (`hjkl`/arrows, `Enter`, `Esc`, `Backspace`).

![idxd demo screenshot](assets/demo.jpg)

## Stack

- Language: Rust
- UI: GTK4
- Media/image handling: `image`, `libmpv2`
- Runtime/utilities: `tokio`, `tracing`
- Data/cache foundation: `rusqlite`

## Build and Run

```bash
cargo run --release -- /path/to/media
```

## Benchmark (Scan Phase)

Run scan benchmark and emit JSON report under `target/idxd-bench/`:

```bash
cargo run -- --benchmark --path /path/to/media --runs 5
```

This benchmark now includes:
- Full directory scan + metadata cache pass
- Visible-first thumbnail generation with deferred background precompute
- Configurable worker concurrency and resize mode for throughput tuning
- Per-stage thumbnail timing (queue wait, worker, decode, resize, encode)
- Layout/scroll simulation metrics as a CPU-side UI snappiness proxy

Cold-cache runs (clears benchmark DB before each run):

```bash
cargo run -- --benchmark --path /path/to/media --runs 5 --cold-cache
```

Limit thumbnail pass to first N images (default is all images):

```bash
cargo run -- --benchmark --path /path/to/media --runs 3 --thumb-limit 500
```

Optional per-thumbnail timeout (ms) to skip pathological files:

```bash
cargo run -- --benchmark --path /path/to/media --runs 3 --thumb-timeout-ms 5000
```

Tune thumbnail workers / initial visible workload:

```bash
cargo run -- --benchmark --path /path/to/media --runs 1 --thumb-workers 6 --thumb-visible-count 48
```

Use faster resize mode:

```bash
cargo run -- --benchmark --path /path/to/media --runs 1 --thumb-fast-resize
```

Enable optional GPU telemetry sampling (Linux sysfs-based):

```bash
cargo run -- --benchmark --path /path/to/media --runs 3 --gpu-telemetry
```

For NVIDIA GPUs, benchmark telemetry automatically falls back to `nvidia-smi` when sysfs utilization/memory counters are unavailable.

Enable NVIDIA offload scaffolding flag (currently falls back to CPU path if no offload backend is configured):

```bash
cargo run -- --benchmark --path /path/to/media --runs 1 --thumb-nv-offload
```

Customize telemetry sample interval (ms):

```bash
cargo run -- --benchmark --path /path/to/media --runs 3 --gpu-telemetry --gpu-sample-ms 100
```

Run an automated sweep and rank configs:

```bash
./scripts/bench_sweep.py /path/to/media --cold-cache
```

## Flatpak

Build and install locally:

```bash
flatpak install -y flathub org.gnome.Platform//48 org.gnome.Sdk//48 org.freedesktop.Sdk.Extension.rust-stable//24.08
flatpak-builder --user --install --force-clean build-dir flatpak/lt.gtw.idxd.json
```

Run:

```bash
flatpak run lt.gtw.idxd /path/to/media
```

## Project Status

Vibe coded junk to solve a local pain (sorting through lots of fuji photos/vids into tags/status).

## License

MIT with Do No Evil provision.
Some code was generated with AI assistance; no warranty; if you believe it infringes, create issue; I’ll replace/remove.

Ciao
