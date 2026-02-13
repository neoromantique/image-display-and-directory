#!/usr/bin/env python3
import argparse
import csv
import glob
import itertools
import json
import os
import subprocess
import sys
import time
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import List


@dataclass
class SweepResult:
    workers: int
    visible_count: int
    fast_resize: bool
    nv_offload: bool
    elapsed_ms: float
    thumb_visible_ms: float
    thumb_end_to_end_p95_ms: float
    thumb_worker_p95_ms: float
    thumb_decode_p95_ms: float
    thumb_resize_p95_ms: float
    thumb_encode_p95_ms: float
    files_per_sec: float
    report_path: str


def parse_int_list(raw: str) -> List[int]:
    vals = []
    for part in raw.split(","):
        part = part.strip()
        if not part:
            continue
        vals.append(int(part))
    if not vals:
        raise ValueError("empty list")
    return vals


def run_cmd(cmd: List[str], cwd: Path) -> None:
    proc = subprocess.run(cmd, cwd=cwd)
    if proc.returncode != 0:
        raise RuntimeError(f"command failed ({proc.returncode}): {' '.join(cmd)}")


def newest_report(after_ts: float, bench_dir: Path) -> Path:
    candidates = []
    for p in glob.glob(str(bench_dir / "scan-*.json")):
        st = os.stat(p)
        if st.st_mtime >= after_ts - 0.001:
            candidates.append((st.st_mtime, Path(p)))
    if not candidates:
        raise RuntimeError("no benchmark report json produced")
    candidates.sort(key=lambda x: x[0], reverse=True)
    return candidates[0][1]


def load_result(path: Path, workers: int, visible_count: int, fast_resize: bool, nv_offload: bool) -> SweepResult:
    data = json.loads(path.read_text())
    run = data["results"][0]
    return SweepResult(
        workers=workers,
        visible_count=visible_count,
        fast_resize=fast_resize,
        nv_offload=nv_offload,
        elapsed_ms=float(run.get("elapsed_ms", 0.0)),
        thumb_visible_ms=float(run.get("thumb_time_to_visible_ms", 0.0)),
        thumb_end_to_end_p95_ms=float(run.get("thumb_end_to_end_p95_ms", 0.0)),
        thumb_worker_p95_ms=float(run.get("thumb_worker_p95_ms", 0.0)),
        thumb_decode_p95_ms=float(run.get("thumb_decode_p95_ms", 0.0)),
        thumb_resize_p95_ms=float(run.get("thumb_resize_p95_ms", 0.0)),
        thumb_encode_p95_ms=float(run.get("thumb_encode_p95_ms", 0.0)),
        files_per_sec=float(run.get("files_per_sec", 0.0)),
        report_path=str(path),
    )


def main() -> int:
    parser = argparse.ArgumentParser(description="Sweep idxd benchmark tuning knobs and rank results")
    parser.add_argument("path", help="media directory path")
    parser.add_argument("--runs", type=int, default=1, help="benchmark runs per config (default: 1)")
    parser.add_argument("--workers", default="2,4,6,8", help="comma list for --thumb-workers")
    parser.add_argument("--visible", default="24,48,96", help="comma list for --thumb-visible-count")
    parser.add_argument("--thumb-limit", type=int, default=0, help="0 means all images")
    parser.add_argument("--thumb-timeout-ms", type=int, default=0)
    parser.add_argument("--include-nv-offload", action="store_true", help="also test --thumb-nv-offload")
    parser.add_argument("--cold-cache", action="store_true", help="pass --cold-cache for each config")
    parser.add_argument("--gpu-telemetry", action="store_true", help="pass --gpu-telemetry")
    parser.add_argument("--gpu-sample-ms", type=int, default=200)
    parser.add_argument("--max-configs", type=int, default=0, help="limit number of configs (0 = all)")
    parser.add_argument("--output", default="", help="optional csv output path")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    repo = Path(__file__).resolve().parents[1]
    bench_dir = repo / "target" / "idxd-bench"
    bench_dir.mkdir(parents=True, exist_ok=True)

    workers = parse_int_list(args.workers)
    visible_vals = parse_int_list(args.visible)
    fast_vals = [False, True]
    offload_vals = [False, True] if args.include_nv_offload else [False]

    configs = list(itertools.product(workers, visible_vals, fast_vals, offload_vals))
    if args.max_configs > 0:
        configs = configs[: args.max_configs]

    print(f"Running {len(configs)} configs")
    results: List[SweepResult] = []

    for idx, (w, v, fast, offload) in enumerate(configs, start=1):
        cmd = [
            "cargo",
            "run",
            "--release",
            "--",
            "--benchmark",
            "--path",
            args.path,
            "--runs",
            str(args.runs),
            "--thumb-workers",
            str(w),
            "--thumb-visible-count",
            str(v),
            "--thumb-limit",
            str(args.thumb_limit),
            "--thumb-timeout-ms",
            str(args.thumb_timeout_ms),
        ]
        if fast:
            cmd.append("--thumb-fast-resize")
        if offload:
            cmd.append("--thumb-nv-offload")
        if args.cold_cache:
            cmd.append("--cold-cache")
        if args.gpu_telemetry:
            cmd.extend(["--gpu-telemetry", "--gpu-sample-ms", str(args.gpu_sample_ms)])

        print(
            f"[{idx}/{len(configs)}] workers={w} visible={v} fast={fast} nv_offload={offload}",
            flush=True,
        )

        if args.dry_run:
            print(" ".join(cmd))
            continue

        t0 = time.time()
        run_cmd(cmd, repo)
        report = newest_report(t0, bench_dir)
        results.append(load_result(report, w, v, fast, offload))

    if args.dry_run:
        return 0

    if not results:
        print("No results collected", file=sys.stderr)
        return 1

    results.sort(key=lambda r: (r.thumb_visible_ms, r.elapsed_ms, -r.files_per_sec))

    out_path = Path(args.output) if args.output else bench_dir / f"sweep-{int(time.time())}.csv"
    with out_path.open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=list(asdict(results[0]).keys()))
        writer.writeheader()
        for row in results:
            writer.writerow(asdict(row))

    best = results[0]
    print("\nTop 5 configs (by visible-ms, then elapsed):")
    for row in results[:5]:
        print(
            f" workers={row.workers} visible={row.visible_count} fast={row.fast_resize} offload={row.nv_offload}"
            f" visible_ms={row.thumb_visible_ms:.1f} elapsed_ms={row.elapsed_ms:.1f}"
            f" p95={row.thumb_end_to_end_p95_ms:.1f} files_per_sec={row.files_per_sec:.1f}"
        )

    print("\nBest config:")
    print(
        f" workers={best.workers} visible={best.visible_count} fast={best.fast_resize} offload={best.nv_offload}"
    )
    print(f" visible_ms={best.thumb_visible_ms:.1f} elapsed_ms={best.elapsed_ms:.1f}")
    print(f" report={best.report_path}")
    print(f" csv={out_path}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
