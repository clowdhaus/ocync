# bench

Benchmark infrastructure for comparing ocync against other OCI sync tools (dregsy, regsync).

- `proxy/` -- Rust MITM proxy for capturing HTTP traffic during benchmark runs
- `results/` -- Benchmark output: per-run summaries and historical JSON run records
- `terraform/` -- Cloud infrastructure (EC2 + ECR) for running benchmarks
- `corpus.yaml` -- Image corpus definition for benchmark workloads
- `corpus-small.yaml` -- 53 small images (about 20 MB or less each), for per-request overhead
- `corpus-large.yaml` -- 6 multi-arch images of 9 to 25 GB, for sustained throughput and very large blobs
- `corpus*.lock.json` -- The lock beside each corpus, written by `cargo xtask bench-lock --corpus <file>`
- `corpus-partial-overrides.yaml` -- Tag overrides for partial sync scenario (~5% churn)

See `bench/CLAUDE.md` or `xtask/` for usage instructions. See the [performance documentation](https://clowdhaus.github.io/ocync/performance) for benchmark results and methodology.
