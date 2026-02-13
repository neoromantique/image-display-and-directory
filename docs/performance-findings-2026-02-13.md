# Performance Findings (2026-02-13)

## Scope
This document captures important performance findings and implemented changes for `idxd` related to scrolling responsiveness, viewer/grid behavior, and thumbnail pipeline throughput.

## Key Findings

1. Main bottleneck is thumbnail generation, not layout or scanner.
- Typical cold benchmark profile showed thumbnail phase dominating total runtime.
- Layout p95 stayed near zero in benchmark runs.

2. Cached runs are very fast.
- Warm cache runs mostly hit metadata + thumbnail cache and complete quickly.
- Cold and warm runs must be evaluated separately.

3. NVIDIA telemetry via sysfs alone was incomplete in this environment.
- Added `nvidia-smi` fallback for GPU utilization and VRAM metrics.

4. Best benchmark sweep settings on this machine (from sweep output):
- `workers=8`
- `visible=24`
- `fast_resize=true`
- `nv_offload=true` gave slight edge vs off on sweep ranking, but offload backend is still scaffolded (see caveats).

## Implemented Changes

### App behavior and responsiveness

1. Preserve scroll position across viewer open/close.
- Grid scroll position is saved before opening viewer.
- On close, scroll is restored immediately and reinforced via idle + short timeout callbacks.

2. Keep UI responsive while images load.
- Row preview decoder threads increased/adapted for throughput.
- Main-thread result application is capped per tick to avoid long UI stalls when many thumbnails finish simultaneously.
- Row preview resize path uses faster filter (`Triangle`) and explicit scaling.

3. Prefetch radius increased.
- Selection prefetch radius increased from 12 to 24 to improve near-neighbor readiness.

### Benchmarking system

1. Added staged/visible-first benchmark model (`schema_version: 4`).
- Measures time-to-visible separately from full background completion.

2. Added tuning flags:
- `--thumb-workers`
- `--thumb-visible-count`
- `--thumb-fast-resize`
- `--thumb-timeout-ms`
- `--thumb-nv-offload` (scaffold)

3. Added per-stage timing metrics:
- queue wait
- worker time
- decode
- resize
- encode
- p95 and avg aggregates

4. Added GPU telemetry fallback:
- Uses `nvidia-smi` when sysfs counters are missing for NVIDIA.

5. Added sweep automation:
- `scripts/bench_sweep.py`
- `just bench-sweep <path>`
- Produces ranked configs and CSV output.

## Operational Runbook

### Recommended benchmark command

```bash
cargo run --release -- --benchmark --path /path/to/media --runs 1 --thumb-workers 8 --thumb-visible-count 24 --thumb-fast-resize --gpu-telemetry
```

### Sweep command

```bash
./scripts/bench_sweep.py /path/to/media --cold-cache
```

### Just recipes

```bash
just bench-clean
just bench /path/to/media 1
just bench-gpu /path/to/media 1 200
just bench-tuned /path/to/media 1 8 24
just bench-sweep /path/to/media
```

## Caveats / Open Items

1. `--thumb-nv-offload` is currently scaffolding.
- It reports enabled/available/attempted/used, but does not yet execute a true GPU thumbnail decode/resize pipeline.

2. Sweep optimum is machine- and dataset-dependent.
- Keep sweeps per target hardware and media characteristics.

3. Some benchmark metrics are benchmark-pipeline specific and not 1:1 with user-perceived UI feel.
- The most useful user-facing indicator remains `thumb_time_to_visible_ms` under representative workloads.

## Files touched (major)
- `src/ui/window.rs`
- `src/ui/list_view.rs`
- `src/ui/row_widget.rs`
- `src/thumbnails/generator.rs`
- `src/bench/mod.rs`
- `scripts/bench_sweep.py`
- `README.md`
- `justfile`
