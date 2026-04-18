# bench

Benchmark infrastructure for comparing ocync against other OCI sync tools (dregsy, regsync).

- `proxy/` -- Rust MITM proxy for capturing HTTP traffic during benchmark runs
- `terraform/` -- AWS infrastructure (EC2 + ECR) for running benchmarks
- `corpus.yaml` -- Image corpus definition for benchmark workloads

See the [performance documentation](https://clowdhaus.github.io/ocync/performance) for benchmark results and methodology.
