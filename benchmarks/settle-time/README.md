# AFT settle-time benchmark

Docker-only benchmark for measuring how long the `aft` bridge takes to settle after
`configure`: search index build, local fastembed semantic index build, watcher-driven
Tier-2 inspect scans, process-tree CPU, and RSS.

The wrapper reuses the existing Linux build path (`tests/docker/Dockerfile.build-linux`),
extracts the release `aft` binary, builds a small runtime image with ONNX Runtime, clones
the target GitHub repo inside the container, drives `aft` over NDJSON stdin/stdout, and
samples `/proc` periodically (5s by default).

## Run

```bash
cd benchmarks/settle-time
./run.sh <github-url> [ref]
```

Examples:

```bash
./run.sh https://github.com/honojs/hono.git main
AFT_SETTLE_SEMANTIC=off ./run.sh https://github.com/honojs/hono.git main
```

Useful environment knobs:

- `AFT_SETTLE_SEMANTIC=on|off` — default `on`; `off` measures search index + Tier-2 only.
- `AFT_SETTLE_IDLE_SECS=30` — sustained idle window required after phases finish.
- `AFT_SETTLE_CPU_THRESHOLD=5` — process-tree CPU must stay below this percent of one core.
- `AFT_SETTLE_TIMEOUT_SECS=1800` — hard timeout for one repo run (use 30m caps for regression triage).
- `AFT_SETTLE_PLATFORM=linux/amd64` — Docker platform used for build and runtime.
- `AFT_SETTLE_SAMPLE_INTERVAL=5` — process-tree CPU/RSS sample cadence.
- `AFT_SETTLE_PROGRESS_INTERVAL=30` — print incremental phase timing/cpu/rss updates.
- `AFT_SETTLE_CLEAR_STORAGE=1` — clear per-run AFT storage (cold index) while keeping `/cache/fastembed` model files.

Outputs are written under `benchmarks/settle-time/results/<repo>--<sha>--semantic-{on,off}/`:

- `summary.json` — machine-readable metrics and timeline.
- `samples.csv` — process-tree CPU/RSS samples with phase labels at `AFT_SETTLE_SAMPLE_INTERVAL`.
- `aft.log` — timestamped AFT stderr logs.
- `aft-stdout.ndjson` — raw protocol responses and push frames.

Settle is defined as: search terminal + semantic terminal (`ready`, `disabled`, or `failed`) +
all three Tier-2 categories completed + process-tree CPU below threshold for the sustained idle window.
The harness uses the default watcher-driven `ConfigureWarm` Tier-2 scan path; it does **not**
force Tier-2 via `inspect_tier2_run`.
