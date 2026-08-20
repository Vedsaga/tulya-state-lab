# tulya-state-lab

Disposable Rust research harness for selecting Tulya's state representation.

**This is not the Tulya storage engine.** If code does not help compare state representations, it does not belong here.

The lab exists to answer one question:

> Does a branch-native persistent representation materially improve the storage and access economics of branch-heavy agent state compared with simple content sharing?

## Phase 1

The first experiment compares two deliberately different adversaries under the same deterministic branch/edit/read plan:

- **persistent AVL rope** — immutable `Arc` nodes, path-copying split/join, AVL rebalancing, chunked leaves, direct range traversal;
- **simple CDC dedup** — rolling-hash content-defined chunks, exact-byte collision verification, deduplicated payload storage, per-version manifests with binary-searched range reads. Edits intentionally reconstruct and re-chunk the parent state, making this a simple storage adversary rather than an optimized dynamic CDC implementation.

Every generated child chooses an arbitrary historical parent. Both backends must retain all version handles. Sampled versions are fully decoded and compared byte-for-byte after timing.

The repository intentionally excludes durability/WAL, networking, auth, crypto, framework integrations, production APIs, and Lean↔Rust refinement. Those only become relevant if a representation survives the benchmark.

## What is measured

The harness reports:

- build time;
- edit latency p50/p95/p99;
- range-read latency p50/p95/p99;
- retained payload bytes;
- retained explicit metadata bytes;
- retained growth per branch;
- lifetime allocation/write amplification per branch;
- live versus lifetime-allocated structural objects;
- a read checksum plus sampled cross-backend semantic equality.

Storage numbers are **estimates**, not RSS or filesystem bytes. They count payload plus explicit nodes/manifests. Allocator control blocks, hash-table bucket capacity, and process/runtime overhead are excluded. This limitation applies to both backends and is printed by the benchmark.

## Kill rule

A representation is not interesting merely because it is elegant, persistent, compressed, or formally provable.

Do not promote the AVL design into a production engine merely because it beats the deliberately simple CDC implementation on edit CPU. The useful signal is a **large combined advantage** on branch-heavy workloads after retained metadata, write amplification, historical range reads, and realistic state shapes are counted.

If the AVL rope is close to CDC on retained growth, or if its storage win is purchased with materially worse reads/metadata, it is only a baseline. If a later incremental CDC/COW implementation comes within roughly 20–25% of the winning design on the intended workload with much less complexity, prefer the simpler design.

## Run locally

Requires stable Rust. There are no third-party Rust dependencies.

```bash
cargo test --all-targets
cargo run --release -- --branches 1000 --base-mib 2
```

That is the first smoke-scale run. After it is healthy, scale the exact same harness:

```bash
cargo run --release -- \
  --branches 10000 \
  --base-mib 8 \
  --edit-bytes 96 \
  --read-bytes 4096 \
  --leaf-bytes 4096 \
  --avg-chunk-bytes 4096 \
  --verify-samples 32
```

The simple CDC backend currently scans/re-chunks the full parent on each edit, so the 10k × 8 MiB run is intentionally expensive. Do not interpret that CPU gap as product novelty; retained growth and read behavior are the more useful Phase-1 signals for deciding what to prototype next.

Use `cargo run --release -- --help` for all options.

## Scope discipline

No server. No CLI product. No database. No WAL. No crypto. No agent SDK. No production persistence. No benchmark-specific shortcuts in a backend.

If a feature does not help falsify or compare state-representation choices, it stays out of this repository.
