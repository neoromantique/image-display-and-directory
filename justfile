flatpak-install:
    flatpak-builder --user --install --force-clean build-dir flatpak/lt.gtw.idxd.json

bench-clean:
    rm -rf target/idxd-bench/thumbs
    rm -f target/idxd-bench/cache.sqlite target/idxd-bench/cache.sqlite-wal target/idxd-bench/cache.sqlite-shm

bench media_path runs='1':
    just bench-clean
    runs_val="{{runs}}"; runs_val="${runs_val#runs=}"; \
    cargo run --release -- --benchmark --path "{{media_path}}" --runs "$runs_val"

bench-gpu media_path runs='1' sample_ms='200':
    just bench-clean
    runs_val="{{runs}}"; runs_val="${runs_val#runs=}"; \
    sample_val="{{sample_ms}}"; sample_val="${sample_val#sample_ms=}"; \
    cargo run --release -- --benchmark --path "{{media_path}}" --runs "$runs_val" --gpu-telemetry --gpu-sample-ms "$sample_val"

bench-tuned media_path runs='1' workers='6' visible='48':
    just bench-clean
    runs_val="{{runs}}"; runs_val="${runs_val#runs=}"; \
    workers_val="{{workers}}"; workers_val="${workers_val#workers=}"; \
    visible_val="{{visible}}"; visible_val="${visible_val#visible=}"; \
    cargo run --release -- --benchmark --path "{{media_path}}" --runs "$runs_val" --thumb-workers "$workers_val" --thumb-visible-count "$visible_val" --thumb-fast-resize

bench-sweep media_path:
    ./scripts/bench_sweep.py "{{media_path}}" --cold-cache
