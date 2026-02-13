use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::layout::justified::JustifiedLayout;
use crate::models::{MediaItem, MediaStore};
use crate::scanner::file_scanner::FileScanner;
use crate::thumbnails::cache::CacheKey;
use crate::thumbnails::generator::{ResizeMode, ThumbnailGenerator};

#[derive(Debug, Clone)]
pub struct BenchmarkArgs {
    pub path: PathBuf,
    pub runs: usize,
    pub cold_cache: bool,
    pub thumb_limit: usize,
    pub thumb_timeout_ms: u64,
    pub thumb_workers: usize,
    pub thumb_visible_count: usize,
    pub thumb_fast_resize: bool,
    pub thumb_nv_offload: bool,
    pub gpu_telemetry: bool,
    pub gpu_sample_ms: u64,
}

#[derive(Debug)]
struct BenchmarkRun {
    run_index: usize,
    elapsed_ms: u128,
    total_files: usize,
    new_items: usize,
    cached_items: usize,
    error_count: usize,
    files_per_sec: f64,
    load_items_ms: u128,
    loaded_items: usize,
    layout_rows: usize,
    layout_total_ms: u128,
    layout_frames_simulated: usize,
    layout_frame_p50_ms: f64,
    layout_frame_p95_ms: f64,
    layout_frames_over_16ms: usize,
    layout_frames_over_33ms: usize,
    thumb_images_total: usize,
    thumb_images_selected: usize,
    thumb_images_visible: usize,
    thumb_images_generated: usize,
    thumb_images_skipped_cached: usize,
    thumb_images_failed: usize,
    thumb_total_ms: u128,
    thumb_time_to_visible_ms: u128,
    thumb_end_to_end_avg_ms: f64,
    thumb_end_to_end_p95_ms: f64,
    thumb_queue_wait_avg_ms: f64,
    thumb_queue_wait_p95_ms: f64,
    thumb_worker_avg_ms: f64,
    thumb_worker_p95_ms: f64,
    thumb_decode_avg_ms: f64,
    thumb_decode_p95_ms: f64,
    thumb_resize_avg_ms: f64,
    thumb_resize_p95_ms: f64,
    thumb_encode_avg_ms: f64,
    thumb_encode_p95_ms: f64,
    thumb_workers: usize,
    thumb_resize_mode: String,
    thumb_nv_offload_enabled: bool,
    thumb_nv_offload_available: bool,
    thumb_nv_offload_attempted: usize,
    thumb_nv_offload_used: usize,
    gpu: Option<GpuRunSummary>,
}

#[derive(Debug)]
struct BenchmarkAggregate {
    runs: usize,
    avg_elapsed_ms: f64,
    min_elapsed_ms: u128,
    max_elapsed_ms: u128,
    avg_files_per_sec: f64,
    avg_layout_p95_ms: f64,
    avg_thumb_p95_ms: f64,
    avg_thumb_visible_ms: f64,
}

#[derive(Debug)]
struct BenchmarkReport {
    schema_version: u32,
    generated_at_unix_ms: u128,
    benchmark: String,
    path: String,
    runs_requested: usize,
    cold_cache: bool,
    thumb_limit: usize,
    thumb_timeout_ms: u64,
    thumb_workers: usize,
    thumb_visible_count: usize,
    thumb_fast_resize: bool,
    thumb_nv_offload: bool,
    gpu_telemetry_enabled: bool,
    gpu_sample_ms: u64,
    db_path: String,
    thumbs_dir: String,
    results: Vec<BenchmarkRun>,
    aggregate: BenchmarkAggregate,
}

#[derive(Debug, Clone)]
struct GpuDeviceSnapshot {
    card: String,
    vendor: String,
    busy_percent: Option<f64>,
    vram_used_bytes: Option<u64>,
    vram_total_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
struct GpuSample {
    t_ms: u128,
    devices: Vec<GpuDeviceSnapshot>,
}

#[derive(Debug)]
struct GpuDeviceSummary {
    card: String,
    vendor: String,
    samples: usize,
    avg_busy_percent: Option<f64>,
    max_busy_percent: Option<f64>,
    max_vram_used_bytes: Option<u64>,
    max_vram_util_percent: Option<f64>,
}

#[derive(Debug)]
struct GpuRunSummary {
    sample_count: usize,
    devices: Vec<GpuDeviceSummary>,
    collection_error: Option<String>,
}

#[derive(Debug, Clone)]
struct NvidiaSmiSample {
    gpu_util_percent: f64,
    memory_used_bytes: u64,
    memory_total_bytes: u64,
}

#[derive(Debug, Clone)]
struct ThumbTask {
    path: PathBuf,
    mtime: i64,
    size: i64,
    enqueued_at: Instant,
}

#[derive(Debug, Clone)]
struct ThumbResult {
    path: PathBuf,
    cache_hit: bool,
    success: bool,
    queue_wait_ms: f64,
    worker_ms: f64,
    end_to_end_ms: f64,
    decode_ms: f64,
    resize_ms: f64,
    encode_ms: f64,
    offload_attempted: bool,
    offload_used: bool,
}

#[derive(Debug, Clone)]
struct ThumbOffloadConfig {
    enabled: bool,
    available: bool,
}

struct GpuTelemetryCollector {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<GpuSample>>>,
    error: Arc<Mutex<Option<String>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl GpuTelemetryCollector {
    fn start(sample_ms: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(Mutex::new(Vec::new()));
        let error = Arc::new(Mutex::new(None));

        let stop_ref = Arc::clone(&stop);
        let samples_ref = Arc::clone(&samples);
        let error_ref = Arc::clone(&error);
        let interval = Duration::from_millis(sample_ms.max(20));
        let started = Instant::now();

        let handle = thread::spawn(move || {
            while !stop_ref.load(Ordering::Relaxed) {
                match sample_gpu_devices() {
                    Ok(devices) => {
                        let sample = GpuSample {
                            t_ms: started.elapsed().as_millis(),
                            devices,
                        };
                        if let Ok(mut all) = samples_ref.lock() {
                            all.push(sample);
                        }
                    }
                    Err(e) => {
                        if let Ok(mut err) = error_ref.lock() {
                            if err.is_none() {
                                *err = Some(e.to_string());
                            }
                        }
                    }
                }
                thread::sleep(interval);
            }
        });

        Self {
            stop,
            samples,
            error,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> GpuRunSummary {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }

        let samples = self
            .samples
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| Vec::new());
        let error = self.error.lock().ok().and_then(|e| e.clone());

        summarize_gpu_samples(&samples, error)
    }
}

pub fn maybe_parse_args() -> Result<Option<BenchmarkArgs>> {
    let mut benchmark = false;
    let mut path: Option<PathBuf> = None;
    let mut runs: usize = 1;
    let mut cold_cache = false;
    let mut thumb_limit: usize = 0;
    let mut thumb_timeout_ms: u64 = 0;
    let mut thumb_workers: usize = 2;
    let mut thumb_visible_count: usize = 24;
    let mut thumb_fast_resize = false;
    let mut thumb_nv_offload = false;
    let mut gpu_telemetry = false;
    let mut gpu_sample_ms: u64 = 200;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--benchmark" => benchmark = true,
            "--path" => {
                let value = args
                    .next()
                    .context("Missing value for --path in benchmark mode")?;
                path = Some(PathBuf::from(value));
            }
            "--runs" => {
                let value = args
                    .next()
                    .context("Missing value for --runs in benchmark mode")?;
                runs = value
                    .parse::<usize>()
                    .context("Failed to parse --runs as a positive integer")?;
            }
            "--thumb-limit" => {
                let value = args
                    .next()
                    .context("Missing value for --thumb-limit in benchmark mode")?;
                thumb_limit = value
                    .parse::<usize>()
                    .context("Failed to parse --thumb-limit as a non-negative integer")?;
            }
            "--thumb-timeout-ms" => {
                let value = args
                    .next()
                    .context("Missing value for --thumb-timeout-ms in benchmark mode")?;
                thumb_timeout_ms = value
                    .parse::<u64>()
                    .context("Failed to parse --thumb-timeout-ms as a non-negative integer")?;
            }
            "--thumb-workers" => {
                let value = args
                    .next()
                    .context("Missing value for --thumb-workers in benchmark mode")?;
                thumb_workers = value
                    .parse::<usize>()
                    .context("Failed to parse --thumb-workers as a positive integer")?;
            }
            "--thumb-visible-count" => {
                let value = args
                    .next()
                    .context("Missing value for --thumb-visible-count in benchmark mode")?;
                thumb_visible_count = value
                    .parse::<usize>()
                    .context("Failed to parse --thumb-visible-count as a non-negative integer")?;
            }
            "--thumb-fast-resize" => thumb_fast_resize = true,
            "--thumb-nv-offload" => thumb_nv_offload = true,
            "--gpu-telemetry" => gpu_telemetry = true,
            "--gpu-sample-ms" => {
                let value = args
                    .next()
                    .context("Missing value for --gpu-sample-ms in benchmark mode")?;
                gpu_sample_ms = value
                    .parse::<u64>()
                    .context("Failed to parse --gpu-sample-ms as a positive integer")?;
            }
            "--cold-cache" => cold_cache = true,
            _ => {
                if benchmark && path.is_none() && !arg.starts_with('-') {
                    path = Some(PathBuf::from(arg));
                }
            }
        }
    }

    if !benchmark {
        return Ok(None);
    }
    if runs == 0 {
        bail!("--runs must be greater than 0");
    }
    if gpu_sample_ms == 0 {
        bail!("--gpu-sample-ms must be greater than 0");
    }
    if thumb_workers == 0 {
        bail!("--thumb-workers must be greater than 0");
    }

    let path = path.context("Benchmark mode requires --path <directory> (or positional path)")?;
    Ok(Some(BenchmarkArgs {
        path,
        runs,
        cold_cache,
        thumb_limit,
        thumb_timeout_ms,
        thumb_workers,
        thumb_visible_count,
        thumb_fast_resize,
        thumb_nv_offload,
        gpu_telemetry,
        gpu_sample_ms,
    }))
}

pub fn run_benchmark(args: BenchmarkArgs) -> Result<i32> {
    if !args.path.exists() {
        bail!("Benchmark path does not exist: {}", args.path.display());
    }
    if !args.path.is_dir() {
        bail!("Benchmark path is not a directory: {}", args.path.display());
    }

    let output_dir = PathBuf::from("target/idxd-bench");
    let thumbs_dir = output_dir.join("thumbs");
    fs::create_dir_all(&output_dir).context("Failed to create benchmark output directory")?;
    fs::create_dir_all(&thumbs_dir).context("Failed to create benchmark thumbnail directory")?;

    let db_path = output_dir.join("cache.sqlite");
    let mut runs = Vec::with_capacity(args.runs);

    for run_index in 0..args.runs {
        let run_number = run_index + 1;
        println!("run={} phase=begin", run_number);

        if args.cold_cache {
            clear_cache_files(&db_path)?;
            clear_thumb_cache_dir(&thumbs_dir)?;
        }

        let gpu_collector = if args.gpu_telemetry {
            Some(GpuTelemetryCollector::start(args.gpu_sample_ms))
        } else {
            None
        };

        let start = Instant::now();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("Failed to create tokio runtime for benchmark")?;

        println!("run={} phase=scan start", run_number);
        let store = MediaStore::open(&db_path)?;
        let scanner = FileScanner::new();
        let (_items, scan_result) = runtime
            .block_on(scanner.scan_directory(&args.path, store))
            .context("Scan benchmark failed")?;
        println!(
            "run={} phase=scan done total={} new={} cached={} errors={}",
            run_number,
            scan_result.total_files,
            scan_result.new_items,
            scan_result.cached_items,
            scan_result.error_count
        );

        let files_per_sec = if start.elapsed().as_secs_f64() > 0.0 {
            scan_result.total_files as f64 / start.elapsed().as_secs_f64()
        } else {
            scan_result.total_files as f64
        };

        println!("run={} phase=load-items start", run_number);
        let load_start = Instant::now();
        let store = MediaStore::open(&db_path)?;
        let media_items = store
            .get_media_batch(&scan_result.paths)
            .context("Failed to load scanned media items from benchmark DB")?;
        let load_items_ms = load_start.elapsed().as_millis();
        println!(
            "run={} phase=load-items done loaded={} ms={}",
            run_number,
            media_items.len(),
            load_items_ms
        );

        println!("run={} phase=layout start", run_number);
        let (layout_rows, layout_total_ms, frames_simulated, frame_p50, frame_p95, over16, over33) =
            simulate_layout_snappiness(&media_items);
        println!(
            "run={} phase=layout done rows={} frames={} p95_ms={:.2}",
            run_number, layout_rows, frames_simulated, frame_p95
        );

        println!("run={} phase=thumbnails start", run_number);
        let resize_mode = if args.thumb_fast_resize {
            ResizeMode::Fast
        } else {
            ResizeMode::Quality
        };
        let thumb_metrics = run_thumbnail_pass(
            &media_items,
            &thumbs_dir,
            args.thumb_limit,
            args.thumb_visible_count,
            args.thumb_workers,
            args.thumb_timeout_ms,
            resize_mode,
            args.thumb_nv_offload,
            run_number,
        );
        println!(
            "run={} phase=thumbnails done selected={} generated={} cached={} failed={} visible_ms={} p95_ms={:.2}",
            run_number,
            thumb_metrics.images_selected,
            thumb_metrics.images_generated,
            thumb_metrics.images_skipped_cached,
            thumb_metrics.images_failed,
            thumb_metrics.time_to_visible_ms,
            thumb_metrics.end_to_end_p95_ms
        );

        let elapsed_ms = start.elapsed().as_millis();
        let gpu_summary = gpu_collector.map(|collector| collector.finish());

        runs.push(BenchmarkRun {
            run_index: run_number,
            elapsed_ms,
            total_files: scan_result.total_files,
            new_items: scan_result.new_items,
            cached_items: scan_result.cached_items,
            error_count: scan_result.error_count,
            files_per_sec,
            load_items_ms,
            loaded_items: media_items.len(),
            layout_rows,
            layout_total_ms,
            layout_frames_simulated: frames_simulated,
            layout_frame_p50_ms: frame_p50,
            layout_frame_p95_ms: frame_p95,
            layout_frames_over_16ms: over16,
            layout_frames_over_33ms: over33,
            thumb_images_total: thumb_metrics.images_total,
            thumb_images_selected: thumb_metrics.images_selected,
            thumb_images_visible: thumb_metrics.images_visible,
            thumb_images_generated: thumb_metrics.images_generated,
            thumb_images_skipped_cached: thumb_metrics.images_skipped_cached,
            thumb_images_failed: thumb_metrics.images_failed,
            thumb_total_ms: thumb_metrics.total_ms,
            thumb_time_to_visible_ms: thumb_metrics.time_to_visible_ms,
            thumb_end_to_end_avg_ms: thumb_metrics.end_to_end_avg_ms,
            thumb_end_to_end_p95_ms: thumb_metrics.end_to_end_p95_ms,
            thumb_queue_wait_avg_ms: thumb_metrics.queue_wait_avg_ms,
            thumb_queue_wait_p95_ms: thumb_metrics.queue_wait_p95_ms,
            thumb_worker_avg_ms: thumb_metrics.worker_avg_ms,
            thumb_worker_p95_ms: thumb_metrics.worker_p95_ms,
            thumb_decode_avg_ms: thumb_metrics.decode_avg_ms,
            thumb_decode_p95_ms: thumb_metrics.decode_p95_ms,
            thumb_resize_avg_ms: thumb_metrics.resize_avg_ms,
            thumb_resize_p95_ms: thumb_metrics.resize_p95_ms,
            thumb_encode_avg_ms: thumb_metrics.encode_avg_ms,
            thumb_encode_p95_ms: thumb_metrics.encode_p95_ms,
            thumb_workers: args.thumb_workers,
            thumb_resize_mode: match resize_mode {
                ResizeMode::Quality => "quality".to_string(),
                ResizeMode::Fast => "fast".to_string(),
            },
            thumb_nv_offload_enabled: args.thumb_nv_offload,
            thumb_nv_offload_available: thumb_metrics.offload_available,
            thumb_nv_offload_attempted: thumb_metrics.offload_attempted,
            thumb_nv_offload_used: thumb_metrics.offload_used,
            gpu: gpu_summary,
        });
    }

    let aggregate = build_aggregate(&runs);
    let generated_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock appears to be before Unix epoch")?
        .as_millis();
    let output_path = output_dir.join(format!("scan-{}.json", generated_at_unix_ms));

    let report = BenchmarkReport {
        schema_version: 4,
        generated_at_unix_ms,
        benchmark: "scan_layout_thumb_staged_gpu_v4".to_string(),
        path: args.path.to_string_lossy().to_string(),
        runs_requested: args.runs,
        cold_cache: args.cold_cache,
        thumb_limit: args.thumb_limit,
        thumb_timeout_ms: args.thumb_timeout_ms,
        thumb_workers: args.thumb_workers,
        thumb_visible_count: args.thumb_visible_count,
        thumb_fast_resize: args.thumb_fast_resize,
        thumb_nv_offload: args.thumb_nv_offload,
        gpu_telemetry_enabled: args.gpu_telemetry,
        gpu_sample_ms: args.gpu_sample_ms,
        db_path: db_path.to_string_lossy().to_string(),
        thumbs_dir: thumbs_dir.to_string_lossy().to_string(),
        results: runs,
        aggregate,
    };

    let json = render_report_json(&report);
    fs::write(&output_path, json).with_context(|| {
        format!(
            "Failed to write benchmark report to {}",
            output_path.display()
        )
    })?;

    println!("Benchmark complete: {}", output_path.display());
    println!(
        "runs={} avg_ms={:.2} min_ms={} max_ms={} avg_files_per_sec={:.2} avg_layout_p95_ms={:.2} avg_thumb_p95_ms={:.2} avg_thumb_visible_ms={:.2}",
        report.aggregate.runs,
        report.aggregate.avg_elapsed_ms,
        report.aggregate.min_elapsed_ms,
        report.aggregate.max_elapsed_ms,
        report.aggregate.avg_files_per_sec,
        report.aggregate.avg_layout_p95_ms,
        report.aggregate.avg_thumb_p95_ms,
        report.aggregate.avg_thumb_visible_ms
    );

    for run in &report.results {
        println!(
            "run={} elapsed_ms={} scan={}/{}/{}/{} thumbs(selected/vis/gen/cached/fail/workers/mode/visible_ms/p95)={}/{}/{}/{}/{}/{}/{}/{}/{:.2}",
            run.run_index,
            run.elapsed_ms,
            run.total_files,
            run.new_items,
            run.cached_items,
            run.error_count,
            run.thumb_images_selected,
            run.thumb_images_visible,
            run.thumb_images_generated,
            run.thumb_images_skipped_cached,
            run.thumb_images_failed,
            run.thumb_workers,
            run.thumb_resize_mode,
            run.thumb_time_to_visible_ms,
            run.thumb_end_to_end_p95_ms
        );
    }

    Ok(0)
}

#[derive(Debug, Default)]
struct ThumbMetrics {
    images_total: usize,
    images_selected: usize,
    images_visible: usize,
    images_generated: usize,
    images_skipped_cached: usize,
    images_failed: usize,
    total_ms: u128,
    time_to_visible_ms: u128,
    end_to_end_avg_ms: f64,
    end_to_end_p95_ms: f64,
    queue_wait_avg_ms: f64,
    queue_wait_p95_ms: f64,
    worker_avg_ms: f64,
    worker_p95_ms: f64,
    decode_avg_ms: f64,
    decode_p95_ms: f64,
    resize_avg_ms: f64,
    resize_p95_ms: f64,
    encode_avg_ms: f64,
    encode_p95_ms: f64,
    offload_available: bool,
    offload_attempted: usize,
    offload_used: usize,
}

fn run_thumbnail_pass(
    items: &[MediaItem],
    thumbs_dir: &Path,
    thumb_limit: usize,
    thumb_visible_count: usize,
    thumb_workers: usize,
    thumb_timeout_ms: u64,
    resize_mode: ResizeMode,
    nv_offload_enabled: bool,
    run_number: usize,
) -> ThumbMetrics {
    let mut image_items: Vec<&MediaItem> = items
        .iter()
        .filter(|i| ThumbnailGenerator::can_generate(&i.path))
        .collect();
    image_items.sort_by(|a, b| a.path.cmp(&b.path));

    let images_total = image_items.len();
    let selected_count = if thumb_limit == 0 {
        images_total
    } else {
        thumb_limit.min(images_total)
    };
    let visible_count = thumb_visible_count.min(selected_count);

    let mut tasks = Vec::with_capacity(selected_count);
    for item in image_items.into_iter().take(selected_count) {
        tasks.push(ThumbTask {
            path: item.path.clone(),
            mtime: item.mtime,
            size: item.size,
            enqueued_at: Instant::now(),
        });
    }

    let offload_cfg = ThumbOffloadConfig {
        enabled: nv_offload_enabled,
        available: nv_offload_enabled && nvidia_smi_available(),
    };

    let start_all = Instant::now();
    let mut results = Vec::with_capacity(selected_count);

    let phase_visible = run_thumbnail_phase(
        tasks[..visible_count].to_vec(),
        thumb_workers,
        thumb_timeout_ms,
        thumbs_dir.to_path_buf(),
        resize_mode,
        offload_cfg.clone(),
        run_number,
        "visible",
    );
    let time_to_visible_ms = start_all.elapsed().as_millis();
    results.extend(phase_visible);

    let phase_background = if visible_count < selected_count {
        run_thumbnail_phase(
            tasks[visible_count..].to_vec(),
            thumb_workers,
            thumb_timeout_ms,
            thumbs_dir.to_path_buf(),
            resize_mode,
            offload_cfg.clone(),
            run_number,
            "background",
        )
    } else {
        Vec::new()
    };
    results.extend(phase_background);

    let total_ms = start_all.elapsed().as_millis();

    let images_generated = results.iter().filter(|r| r.success && !r.cache_hit).count();
    let images_skipped_cached = results.iter().filter(|r| r.cache_hit).count();
    let images_failed = results.iter().filter(|r| !r.success).count();

    let end_to_end_values: Vec<f64> = results.iter().map(|r| r.end_to_end_ms).collect();
    let queue_wait_values: Vec<f64> = results.iter().map(|r| r.queue_wait_ms).collect();
    let worker_values: Vec<f64> = results.iter().map(|r| r.worker_ms).collect();

    let generated_only: Vec<&ThumbResult> = results
        .iter()
        .filter(|r| r.success && !r.cache_hit)
        .collect();
    let decode_values: Vec<f64> = generated_only.iter().map(|r| r.decode_ms).collect();
    let resize_values: Vec<f64> = generated_only.iter().map(|r| r.resize_ms).collect();
    let encode_values: Vec<f64> = generated_only.iter().map(|r| r.encode_ms).collect();

    let offload_attempted = results.iter().filter(|r| r.offload_attempted).count();
    let offload_used = results.iter().filter(|r| r.offload_used).count();

    ThumbMetrics {
        images_total,
        images_selected: selected_count,
        images_visible: visible_count,
        images_generated,
        images_skipped_cached,
        images_failed,
        total_ms,
        time_to_visible_ms,
        end_to_end_avg_ms: average(&end_to_end_values),
        end_to_end_p95_ms: percentile_ms(&end_to_end_values, 0.95),
        queue_wait_avg_ms: average(&queue_wait_values),
        queue_wait_p95_ms: percentile_ms(&queue_wait_values, 0.95),
        worker_avg_ms: average(&worker_values),
        worker_p95_ms: percentile_ms(&worker_values, 0.95),
        decode_avg_ms: average(&decode_values),
        decode_p95_ms: percentile_ms(&decode_values, 0.95),
        resize_avg_ms: average(&resize_values),
        resize_p95_ms: percentile_ms(&resize_values, 0.95),
        encode_avg_ms: average(&encode_values),
        encode_p95_ms: percentile_ms(&encode_values, 0.95),
        offload_available: offload_cfg.available,
        offload_attempted,
        offload_used,
    }
}

fn run_thumbnail_phase(
    mut tasks: Vec<ThumbTask>,
    workers: usize,
    timeout_ms: u64,
    thumbs_dir: PathBuf,
    resize_mode: ResizeMode,
    offload_cfg: ThumbOffloadConfig,
    run_number: usize,
    phase_name: &str,
) -> Vec<ThumbResult> {
    if tasks.is_empty() {
        return Vec::new();
    }

    let phase_total = tasks.len();
    let (task_tx, task_rx) = flume::bounded::<ThumbTask>(phase_total);
    let (result_tx, result_rx) = flume::unbounded::<ThumbResult>();

    let mut handles = Vec::new();
    let worker_count = workers.clamp(1, 32);

    for _ in 0..worker_count {
        let rx = task_rx.clone();
        let tx = result_tx.clone();
        let phase_thumbs_dir = thumbs_dir.clone();
        let phase_mode = resize_mode;
        let phase_offload = offload_cfg.clone();

        handles.push(thread::spawn(move || {
            while let Ok(task) = rx.recv() {
                let queue_wait_ms = task.enqueued_at.elapsed().as_secs_f64() * 1000.0;
                let worker_start = Instant::now();
                let mut decode_ms = 0.0;
                let mut resize_ms = 0.0;
                let mut encode_ms = 0.0;
                let mut offload_attempted = false;
                let mut offload_used = false;

                let key = CacheKey::new(&task.path, task.mtime, task.size);
                let thumb_path = phase_thumbs_dir.join(key.disk_filename());
                let cache_hit = thumb_path.exists();

                let success = if cache_hit {
                    true
                } else {
                    if phase_offload.enabled && phase_offload.available {
                        offload_attempted = true;
                        // Scaffolding: currently falls back to CPU path after capability check.
                        offload_used = false;
                    }

                    match ThumbnailGenerator::generate_thumbnail_with_mode(
                        &task.path,
                        &thumb_path,
                        256,
                        phase_mode,
                    ) {
                        Ok((_res, timings)) => {
                            decode_ms = timings.decode_ms;
                            resize_ms = timings.resize_ms;
                            encode_ms = timings.encode_ms;
                            true
                        }
                        Err(_) => false,
                    }
                };

                let worker_ms = worker_start.elapsed().as_secs_f64() * 1000.0;
                let result = ThumbResult {
                    path: task.path,
                    cache_hit,
                    success,
                    queue_wait_ms,
                    worker_ms,
                    end_to_end_ms: queue_wait_ms + worker_ms,
                    decode_ms,
                    resize_ms,
                    encode_ms,
                    offload_attempted,
                    offload_used,
                };

                let _ = tx.send(result);
            }
        }));
    }

    for task in &mut tasks {
        task.enqueued_at = Instant::now();
        let _ = task_tx.send(task.clone());
    }
    drop(task_tx);
    drop(result_tx);

    let mut results = Vec::with_capacity(phase_total);
    let mut done = 0usize;
    let progress_every = 10usize;
    let mut last_progress = Instant::now();

    while done < phase_total {
        match result_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(result) => {
                done += 1;
                results.push(result);
                last_progress = Instant::now();

                if done == phase_total || done % progress_every == 0 {
                    println!(
                        "run={} phase=thumbnails:{} progress={}/{}",
                        run_number, phase_name, done, phase_total
                    );
                }
            }
            Err(flume::RecvTimeoutError::Timeout) => {
                if timeout_ms > 0 && last_progress.elapsed() > Duration::from_millis(timeout_ms) {
                    println!(
                        "run={} phase=thumbnails:{} timeout remaining={} timeout_ms={}",
                        run_number,
                        phase_name,
                        phase_total.saturating_sub(done),
                        timeout_ms
                    );
                    break;
                }
            }
            Err(flume::RecvTimeoutError::Disconnected) => break,
        }
    }

    for handle in handles {
        let _ = handle.join();
    }

    results
}

fn nvidia_smi_available() -> bool {
    Command::new("nvidia-smi")
        .arg("-L")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn simulate_layout_snappiness(items: &[MediaItem]) -> (usize, u128, usize, f64, f64, usize, usize) {
    if items.is_empty() {
        return (0, 0, 0, 0.0, 0.0, 0, 0);
    }

    let layout = JustifiedLayout::default();
    let viewport_width = 1920.0;

    let layout_start = Instant::now();
    let rows = layout.compute(items, viewport_width);
    let layout_total_ms = layout_start.elapsed().as_millis();
    let row_count = rows.len();

    let window_size = 180usize.min(items.len());
    let step = (window_size / 6).max(1);
    let mut frame_times_ms = Vec::new();

    let mut idx = 0usize;
    while idx < items.len() {
        let end = (idx + window_size).min(items.len());
        let frame_start = Instant::now();
        let _ = layout.compute(&items[idx..end], viewport_width);
        frame_times_ms.push(frame_start.elapsed().as_secs_f64() * 1000.0);
        if end == items.len() {
            break;
        }
        idx += step;
    }

    let frame_p50 = percentile_ms(&frame_times_ms, 0.50);
    let frame_p95 = percentile_ms(&frame_times_ms, 0.95);
    let over16 = frame_times_ms.iter().filter(|t| **t > 16.67).count();
    let over33 = frame_times_ms.iter().filter(|t| **t > 33.33).count();

    (
        row_count,
        layout_total_ms,
        frame_times_ms.len(),
        frame_p50,
        frame_p95,
        over16,
        over33,
    )
}

fn sample_gpu_devices() -> Result<Vec<GpuDeviceSnapshot>> {
    let drm = Path::new("/sys/class/drm");
    let entries = fs::read_dir(drm)
        .with_context(|| format!("Failed to read GPU telemetry path {}", drm.display()))?;

    let mut devices = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_card_device_name(&name) {
            continue;
        }

        let device_root = entry.path().join("device");
        if !device_root.exists() {
            continue;
        }

        let vendor =
            read_trimmed(device_root.join("vendor")).unwrap_or_else(|_| "unknown".to_string());
        let busy_percent = read_u64(device_root.join("gpu_busy_percent"))
            .ok()
            .map(|v| v as f64);
        let vram_used_bytes = read_u64(device_root.join("mem_info_vram_used")).ok();
        let vram_total_bytes = read_u64(device_root.join("mem_info_vram_total")).ok();

        devices.push(GpuDeviceSnapshot {
            card: name.to_string(),
            vendor,
            busy_percent,
            vram_used_bytes,
            vram_total_bytes,
        });
    }

    if let Ok(smi_samples) = sample_nvidia_smi_devices() {
        apply_nvidia_smi_fallback(&mut devices, &smi_samples);
    }
    if devices.is_empty() {
        if let Ok(smi_samples) = sample_nvidia_smi_devices() {
            devices = nvidia_samples_as_devices(&smi_samples);
        }
    }

    if devices.is_empty() {
        bail!("No GPU devices found in /sys/class/drm and nvidia-smi fallback unavailable");
    }

    Ok(devices)
}

fn sample_nvidia_smi_devices() -> Result<Vec<NvidiaSmiSample>> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .context("Failed to execute nvidia-smi")?;

    if !output.status.success() {
        bail!(
            "nvidia-smi exited with status {}",
            output.status.code().unwrap_or(-1)
        );
    }

    let stdout =
        String::from_utf8(output.stdout).context("nvidia-smi output is not valid UTF-8")?;
    let mut samples = Vec::new();

    for raw_line in stdout.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').map(|p| p.trim()).collect();
        if parts.len() != 3 {
            continue;
        }

        let gpu_util_percent = match parts[0].parse::<f64>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let memory_used_mb = match parts[1].parse::<u64>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let memory_total_mb = match parts[2].parse::<u64>() {
            Ok(v) => v,
            Err(_) => continue,
        };

        samples.push(NvidiaSmiSample {
            gpu_util_percent,
            memory_used_bytes: memory_used_mb.saturating_mul(1024 * 1024),
            memory_total_bytes: memory_total_mb.saturating_mul(1024 * 1024),
        });
    }

    if samples.is_empty() {
        bail!("nvidia-smi returned no parseable GPU rows");
    }

    Ok(samples)
}

fn apply_nvidia_smi_fallback(devices: &mut [GpuDeviceSnapshot], smi_samples: &[NvidiaSmiSample]) {
    let mut nvidia_indices: Vec<usize> = devices
        .iter()
        .enumerate()
        .filter(|(_, d)| is_nvidia_vendor(&d.vendor))
        .map(|(idx, _)| idx)
        .collect();

    nvidia_indices.sort_by_key(|idx| card_number(&devices[*idx].card));

    for (device_idx, smi) in nvidia_indices.into_iter().zip(smi_samples.iter()) {
        let dev = &mut devices[device_idx];
        if dev.busy_percent.is_none() {
            dev.busy_percent = Some(smi.gpu_util_percent);
        }
        if dev.vram_used_bytes.is_none() {
            dev.vram_used_bytes = Some(smi.memory_used_bytes);
        }
        if dev.vram_total_bytes.is_none() {
            dev.vram_total_bytes = Some(smi.memory_total_bytes);
        }
    }
}

fn nvidia_samples_as_devices(smi_samples: &[NvidiaSmiSample]) -> Vec<GpuDeviceSnapshot> {
    smi_samples
        .iter()
        .enumerate()
        .map(|(idx, smi)| GpuDeviceSnapshot {
            card: format!("nvidia{}", idx),
            vendor: "0x10de".to_string(),
            busy_percent: Some(smi.gpu_util_percent),
            vram_used_bytes: Some(smi.memory_used_bytes),
            vram_total_bytes: Some(smi.memory_total_bytes),
        })
        .collect()
}

fn summarize_gpu_samples(samples: &[GpuSample], collection_error: Option<String>) -> GpuRunSummary {
    if samples.is_empty() {
        return GpuRunSummary {
            sample_count: 0,
            devices: Vec::new(),
            collection_error,
        };
    }

    #[derive(Default)]
    struct Acc {
        vendor: String,
        busy_sum: f64,
        busy_count: usize,
        busy_max: f64,
        has_busy: bool,
        vram_max_used: u64,
        has_vram_used: bool,
        vram_max_util: f64,
        has_vram_util: bool,
        sample_count: usize,
    }

    let mut map: BTreeMap<String, Acc> = BTreeMap::new();

    for sample in samples {
        let _ = sample.t_ms;
        for dev in &sample.devices {
            let acc = map.entry(dev.card.clone()).or_default();
            if acc.vendor.is_empty() {
                acc.vendor = dev.vendor.clone();
            }
            acc.sample_count += 1;

            if let Some(busy) = dev.busy_percent {
                acc.busy_sum += busy;
                acc.busy_count += 1;
                if !acc.has_busy || busy > acc.busy_max {
                    acc.busy_max = busy;
                    acc.has_busy = true;
                }
            }

            if let Some(used) = dev.vram_used_bytes {
                if !acc.has_vram_used || used > acc.vram_max_used {
                    acc.vram_max_used = used;
                    acc.has_vram_used = true;
                }
            }

            if let (Some(used), Some(total)) = (dev.vram_used_bytes, dev.vram_total_bytes) {
                if total > 0 {
                    let util = used as f64 * 100.0 / total as f64;
                    if !acc.has_vram_util || util > acc.vram_max_util {
                        acc.vram_max_util = util;
                        acc.has_vram_util = true;
                    }
                }
            }
        }
    }

    let devices = map
        .into_iter()
        .map(|(card, acc)| GpuDeviceSummary {
            card,
            vendor: if acc.vendor.is_empty() {
                "unknown".to_string()
            } else {
                acc.vendor
            },
            samples: acc.sample_count,
            avg_busy_percent: if acc.busy_count > 0 {
                Some(acc.busy_sum / acc.busy_count as f64)
            } else {
                None
            },
            max_busy_percent: if acc.has_busy {
                Some(acc.busy_max)
            } else {
                None
            },
            max_vram_used_bytes: if acc.has_vram_used {
                Some(acc.vram_max_used)
            } else {
                None
            },
            max_vram_util_percent: if acc.has_vram_util {
                Some(acc.vram_max_util)
            } else {
                None
            },
        })
        .collect();

    GpuRunSummary {
        sample_count: samples.len(),
        devices,
        collection_error,
    }
}

fn render_report_json(report: &BenchmarkReport) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"schema_version\": {},\n",
        report.schema_version
    ));
    out.push_str(&format!(
        "  \"generated_at_unix_ms\": {},\n",
        report.generated_at_unix_ms
    ));
    out.push_str(&format!(
        "  \"benchmark\": \"{}\",\n",
        escape_json(&report.benchmark)
    ));
    out.push_str(&format!("  \"path\": \"{}\",\n", escape_json(&report.path)));
    out.push_str(&format!(
        "  \"runs_requested\": {},\n",
        report.runs_requested
    ));
    out.push_str(&format!("  \"cold_cache\": {},\n", report.cold_cache));
    out.push_str(&format!("  \"thumb_limit\": {},\n", report.thumb_limit));
    out.push_str(&format!(
        "  \"thumb_timeout_ms\": {},\n",
        report.thumb_timeout_ms
    ));
    out.push_str(&format!("  \"thumb_workers\": {},\n", report.thumb_workers));
    out.push_str(&format!(
        "  \"thumb_visible_count\": {},\n",
        report.thumb_visible_count
    ));
    out.push_str(&format!(
        "  \"thumb_fast_resize\": {},\n",
        report.thumb_fast_resize
    ));
    out.push_str(&format!(
        "  \"thumb_nv_offload\": {},\n",
        report.thumb_nv_offload
    ));
    out.push_str(&format!(
        "  \"gpu_telemetry_enabled\": {},\n",
        report.gpu_telemetry_enabled
    ));
    out.push_str(&format!("  \"gpu_sample_ms\": {},\n", report.gpu_sample_ms));
    out.push_str(&format!(
        "  \"db_path\": \"{}\",\n",
        escape_json(&report.db_path)
    ));
    out.push_str(&format!(
        "  \"thumbs_dir\": \"{}\",\n",
        escape_json(&report.thumbs_dir)
    ));

    out.push_str("  \"results\": [\n");
    for (idx, run) in report.results.iter().enumerate() {
        out.push_str("    {\n");
        out.push_str(&format!("      \"run_index\": {},\n", run.run_index));
        out.push_str(&format!("      \"elapsed_ms\": {},\n", run.elapsed_ms));
        out.push_str(&format!("      \"total_files\": {},\n", run.total_files));
        out.push_str(&format!("      \"new_items\": {},\n", run.new_items));
        out.push_str(&format!("      \"cached_items\": {},\n", run.cached_items));
        out.push_str(&format!("      \"error_count\": {},\n", run.error_count));
        out.push_str(&format!(
            "      \"files_per_sec\": {:.3},\n",
            run.files_per_sec
        ));
        out.push_str(&format!(
            "      \"load_items_ms\": {},\n",
            run.load_items_ms
        ));
        out.push_str(&format!("      \"loaded_items\": {},\n", run.loaded_items));
        out.push_str(&format!("      \"layout_rows\": {},\n", run.layout_rows));
        out.push_str(&format!(
            "      \"layout_total_ms\": {},\n",
            run.layout_total_ms
        ));
        out.push_str(&format!(
            "      \"layout_frames_simulated\": {},\n",
            run.layout_frames_simulated
        ));
        out.push_str(&format!(
            "      \"layout_frame_p50_ms\": {:.3},\n",
            run.layout_frame_p50_ms
        ));
        out.push_str(&format!(
            "      \"layout_frame_p95_ms\": {:.3},\n",
            run.layout_frame_p95_ms
        ));
        out.push_str(&format!(
            "      \"layout_frames_over_16ms\": {},\n",
            run.layout_frames_over_16ms
        ));
        out.push_str(&format!(
            "      \"layout_frames_over_33ms\": {},\n",
            run.layout_frames_over_33ms
        ));

        out.push_str(&format!(
            "      \"thumb_images_total\": {},\n",
            run.thumb_images_total
        ));
        out.push_str(&format!(
            "      \"thumb_images_selected\": {},\n",
            run.thumb_images_selected
        ));
        out.push_str(&format!(
            "      \"thumb_images_visible\": {},\n",
            run.thumb_images_visible
        ));
        out.push_str(&format!(
            "      \"thumb_images_generated\": {},\n",
            run.thumb_images_generated
        ));
        out.push_str(&format!(
            "      \"thumb_images_skipped_cached\": {},\n",
            run.thumb_images_skipped_cached
        ));
        out.push_str(&format!(
            "      \"thumb_images_failed\": {},\n",
            run.thumb_images_failed
        ));
        out.push_str(&format!(
            "      \"thumb_total_ms\": {},\n",
            run.thumb_total_ms
        ));
        out.push_str(&format!(
            "      \"thumb_time_to_visible_ms\": {},\n",
            run.thumb_time_to_visible_ms
        ));
        out.push_str(&format!(
            "      \"thumb_end_to_end_avg_ms\": {:.3},\n",
            run.thumb_end_to_end_avg_ms
        ));
        out.push_str(&format!(
            "      \"thumb_end_to_end_p95_ms\": {:.3},\n",
            run.thumb_end_to_end_p95_ms
        ));
        out.push_str(&format!(
            "      \"thumb_queue_wait_avg_ms\": {:.3},\n",
            run.thumb_queue_wait_avg_ms
        ));
        out.push_str(&format!(
            "      \"thumb_queue_wait_p95_ms\": {:.3},\n",
            run.thumb_queue_wait_p95_ms
        ));
        out.push_str(&format!(
            "      \"thumb_worker_avg_ms\": {:.3},\n",
            run.thumb_worker_avg_ms
        ));
        out.push_str(&format!(
            "      \"thumb_worker_p95_ms\": {:.3},\n",
            run.thumb_worker_p95_ms
        ));
        out.push_str(&format!(
            "      \"thumb_decode_avg_ms\": {:.3},\n",
            run.thumb_decode_avg_ms
        ));
        out.push_str(&format!(
            "      \"thumb_decode_p95_ms\": {:.3},\n",
            run.thumb_decode_p95_ms
        ));
        out.push_str(&format!(
            "      \"thumb_resize_avg_ms\": {:.3},\n",
            run.thumb_resize_avg_ms
        ));
        out.push_str(&format!(
            "      \"thumb_resize_p95_ms\": {:.3},\n",
            run.thumb_resize_p95_ms
        ));
        out.push_str(&format!(
            "      \"thumb_encode_avg_ms\": {:.3},\n",
            run.thumb_encode_avg_ms
        ));
        out.push_str(&format!(
            "      \"thumb_encode_p95_ms\": {:.3},\n",
            run.thumb_encode_p95_ms
        ));
        out.push_str(&format!(
            "      \"thumb_workers\": {},\n",
            run.thumb_workers
        ));
        out.push_str(&format!(
            "      \"thumb_resize_mode\": \"{}\",\n",
            escape_json(&run.thumb_resize_mode)
        ));
        out.push_str(&format!(
            "      \"thumb_nv_offload_enabled\": {},\n",
            run.thumb_nv_offload_enabled
        ));
        out.push_str(&format!(
            "      \"thumb_nv_offload_available\": {},\n",
            run.thumb_nv_offload_available
        ));
        out.push_str(&format!(
            "      \"thumb_nv_offload_attempted\": {},\n",
            run.thumb_nv_offload_attempted
        ));
        out.push_str(&format!(
            "      \"thumb_nv_offload_used\": {},\n",
            run.thumb_nv_offload_used
        ));

        out.push_str("      \"gpu\": ");
        render_gpu_run_json(&mut out, run.gpu.as_ref(), 6);
        out.push('\n');

        out.push_str("    }");
        if idx + 1 < report.results.len() {
            out.push(',');
        }
        out.push('\n');
    }

    out.push_str("  ],\n");
    out.push_str("  \"aggregate\": {\n");
    out.push_str(&format!("    \"runs\": {},\n", report.aggregate.runs));
    out.push_str(&format!(
        "    \"avg_elapsed_ms\": {:.3},\n",
        report.aggregate.avg_elapsed_ms
    ));
    out.push_str(&format!(
        "    \"min_elapsed_ms\": {},\n",
        report.aggregate.min_elapsed_ms
    ));
    out.push_str(&format!(
        "    \"max_elapsed_ms\": {},\n",
        report.aggregate.max_elapsed_ms
    ));
    out.push_str(&format!(
        "    \"avg_files_per_sec\": {:.3},\n",
        report.aggregate.avg_files_per_sec
    ));
    out.push_str(&format!(
        "    \"avg_layout_p95_ms\": {:.3},\n",
        report.aggregate.avg_layout_p95_ms
    ));
    out.push_str(&format!(
        "    \"avg_thumb_p95_ms\": {:.3},\n",
        report.aggregate.avg_thumb_p95_ms
    ));
    out.push_str(&format!(
        "    \"avg_thumb_visible_ms\": {:.3}\n",
        report.aggregate.avg_thumb_visible_ms
    ));
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

fn render_gpu_run_json(out: &mut String, gpu: Option<&GpuRunSummary>, indent: usize) {
    let pad = " ".repeat(indent);
    let pad2 = " ".repeat(indent + 2);
    let pad3 = " ".repeat(indent + 4);

    if let Some(gpu) = gpu {
        out.push_str("{\n");
        out.push_str(&format!("{pad2}\"sample_count\": {},\n", gpu.sample_count));

        if let Some(err) = &gpu.collection_error {
            out.push_str(&format!(
                "{pad2}\"collection_error\": \"{}\",\n",
                escape_json(err)
            ));
        } else {
            out.push_str(&format!("{pad2}\"collection_error\": null,\n"));
        }

        out.push_str(&format!("{pad2}\"devices\": [\n"));
        for (idx, dev) in gpu.devices.iter().enumerate() {
            out.push_str(&format!("{pad3}{{\n"));
            out.push_str(&format!(
                "{pad3}  \"card\": \"{}\",\n",
                escape_json(&dev.card)
            ));
            out.push_str(&format!(
                "{pad3}  \"vendor\": \"{}\",\n",
                escape_json(&dev.vendor)
            ));
            out.push_str(&format!("{pad3}  \"samples\": {},\n", dev.samples));
            write_optional_f64(
                out,
                &format!("{pad3}  \"avg_busy_percent\": "),
                dev.avg_busy_percent,
                true,
            );
            write_optional_f64(
                out,
                &format!("{pad3}  \"max_busy_percent\": "),
                dev.max_busy_percent,
                true,
            );
            write_optional_u64(
                out,
                &format!("{pad3}  \"max_vram_used_bytes\": "),
                dev.max_vram_used_bytes,
                true,
            );
            write_optional_f64(
                out,
                &format!("{pad3}  \"max_vram_util_percent\": "),
                dev.max_vram_util_percent,
                false,
            );
            out.push_str(&format!("{pad3}}}"));
            if idx + 1 < gpu.devices.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str(&format!("{pad2}]\n"));
        out.push_str(&format!("{pad}}}"));
    } else {
        out.push_str("null");
    }
}

fn build_aggregate(runs: &[BenchmarkRun]) -> BenchmarkAggregate {
    let elapsed_values: Vec<u128> = runs.iter().map(|r| r.elapsed_ms).collect();
    let files_per_sec_values: Vec<f64> = runs.iter().map(|r| r.files_per_sec).collect();
    let layout_p95_values: Vec<f64> = runs.iter().map(|r| r.layout_frame_p95_ms).collect();
    let thumb_p95_values: Vec<f64> = runs.iter().map(|r| r.thumb_end_to_end_p95_ms).collect();
    let thumb_visible_values: Vec<f64> = runs
        .iter()
        .map(|r| r.thumb_time_to_visible_ms as f64)
        .collect();

    let runs_count = runs.len();
    let sum_elapsed: u128 = elapsed_values.iter().sum();
    let avg_elapsed_ms = if runs_count > 0 {
        sum_elapsed as f64 / runs_count as f64
    } else {
        0.0
    };
    let min_elapsed_ms = elapsed_values.iter().copied().min().unwrap_or(0);
    let max_elapsed_ms = elapsed_values.iter().copied().max().unwrap_or(0);

    BenchmarkAggregate {
        runs: runs_count,
        avg_elapsed_ms,
        min_elapsed_ms,
        max_elapsed_ms,
        avg_files_per_sec: average(&files_per_sec_values),
        avg_layout_p95_ms: average(&layout_p95_values),
        avg_thumb_p95_ms: average(&thumb_p95_values),
        avg_thumb_visible_ms: average(&thumb_visible_values),
    }
}

fn is_card_device_name(name: &str) -> bool {
    if !name.starts_with("card") {
        return false;
    }
    if name.contains('-') {
        return false;
    }
    let suffix = &name[4..];
    !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
}

fn is_nvidia_vendor(vendor: &str) -> bool {
    vendor.eq_ignore_ascii_case("0x10de")
}

fn card_number(card: &str) -> u32 {
    if let Some(suffix) = card.strip_prefix("card") {
        return suffix.parse::<u32>().unwrap_or(u32::MAX);
    }
    u32::MAX
}

fn read_trimmed(path: PathBuf) -> Result<String> {
    let raw =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(raw.trim().to_string())
}

fn read_u64(path: PathBuf) -> Result<u64> {
    let raw = read_trimmed(path)?;
    raw.parse::<u64>()
        .with_context(|| format!("Failed to parse numeric value: {raw}"))
}

fn clear_cache_files(db_path: &Path) -> Result<()> {
    let db = db_path.to_string_lossy();
    let candidates = [
        PathBuf::from(db.as_ref()),
        PathBuf::from(format!("{db}-wal")),
        PathBuf::from(format!("{db}-shm")),
    ];

    for candidate in candidates {
        match fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to remove cache file {}", candidate.display())
                });
            }
        }
    }

    Ok(())
}

fn clear_thumb_cache_dir(thumbs_dir: &Path) -> Result<()> {
    if !thumbs_dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(thumbs_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn average(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn percentile_ms(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let clamped = p.clamp(0.0, 1.0);
    let idx = ((sorted.len() - 1) as f64 * clamped).round() as usize;
    sorted[idx]
}

fn write_optional_f64(out: &mut String, prefix: &str, value: Option<f64>, trailing_comma: bool) {
    out.push_str(prefix);
    if let Some(v) = value {
        out.push_str(&format!("{v:.3}"));
    } else {
        out.push_str("null");
    }
    if trailing_comma {
        out.push(',');
    }
    out.push('\n');
}

fn write_optional_u64(out: &mut String, prefix: &str, value: Option<u64>, trailing_comma: bool) {
    out.push_str(prefix);
    if let Some(v) = value {
        out.push_str(&v.to_string());
    } else {
        out.push_str("null");
    }
    if trailing_comma {
        out.push(',');
    }
    out.push('\n');
}

fn escape_json(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped
}
