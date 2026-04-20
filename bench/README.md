# bench

Benchmark infrastructure for comparing ocync against other OCI sync tools (dregsy, regsync).

- `proxy/` -- Rust MITM proxy for capturing HTTP traffic during benchmark runs
- `results/` -- Benchmark output: per-run summaries and historical JSON run records
- `terraform/` -- Cloud infrastructure (EC2 + ECR) for running benchmarks
- `corpus.yaml` -- Image corpus definition for benchmark workloads
- `corpus-partial-overrides.yaml` -- Tag overrides for partial sync scenario (~5% churn)

See `bench/CLAUDE.md` or `xtask/` for usage instructions. See the [performance documentation](https://clowdhaus.github.io/ocync/performance) for benchmark results and methodology.
