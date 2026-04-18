# ocync-sync

Sync orchestration engine - pipelined discovery/execution, leader-follower blob mounting, transfer state cache, and blob staging.

## Concurrency model

- Single-threaded tokio (`current_thread`). Workload is ~100% network I/O.
- All shared state uses `Rc<RefCell<>>`, never `Arc<Mutex<>>`.
- `RefCell` borrows MUST be dropped before any `.await` point.
- The `!Send` constraint of `Rc<RefCell<>>` is a feature: prevents accidental `tokio::spawn`.
- Intra-image blob concurrency: `FuturesUnordered` + local `Semaphore(6)`.

## Engine architecture

- Pipelined: discovery and execution overlap via `tokio::select!` over two `FuturesUnordered` pools.
- `VecDeque<ActiveItem>` pending queue between pools.
- `select!` uses `biased;` (prefer execution completions to free permits).
- Emptiness guards (`if !pool.is_empty()`) on every branch prevent busy-looping.
- Shutdown branch must check work-remaining or it blocks the else exit.

## Leader-follower blob mounting

- Greedy set-cover election in `elect_leaders()` provably covers every shared blob.
- No "uncovered follower" path exists - all followers' shared blobs are in the leader union.
- Do NOT add wave partitioning among followers (dead code).
- Wave 1: blobs covered by leaders. Wave 2: inter-follower deps wait for wave 1 commit.

## Notify contracts (critical)

- `tokio::sync::Notify::notify_waiters()` does NOT store permits.
- Every code path that transitions a blob out of `InProgress` MUST call `notify_blob`.
- Same applies to `BlobStage::notify_staged` / `notify_failed` for source-pull dedup.
- Missing notify = deadlock for concurrent waiters.

## Transfer state cache

- Two-tier: hot (in-memory) + warm (persistent disk, binary postcard + CRC32).
- Only `ExistsAtTarget` and `Completed` persisted; transient states stripped.
- Progressive population: HEAD checks inline during execution, not upfront batch.
- Lazy invalidation: stale entries self-heal on mount/push failure.

## Blob staging

- Content-addressable: `{cache_dir}/blobs/sha256/{hex_digest}`.
- Atomic write: tmp file -> fsync -> rename -> dir fsync.
- Zero overhead for single-target (`BlobStage::disabled()`).
- `claim_or_check` eliminates redundant source GETs (source-pull dedup).

## Testing

- Engine integration tests: pass `Some(&shutdown)` (production path) unless testing termination.
- Mount tests: symmetric mocks (both repos accept upload AND mount) since leader-follower can pick either as leader. Assert aggregate stats, not per-image ordering.
- Concurrency tests: run at `max_concurrent > 1`. Serializing a flaky test hides a race.

## Commands

```bash
# Unit + integration tests
cargo test --package ocync-sync
```
