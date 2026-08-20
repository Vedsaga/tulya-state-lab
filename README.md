# tulya-state-lab

Disposable Rust research harness for selecting Tulya's state representation.

**This is not the Tulya storage engine.** If code does not help compare state representations, it does not belong here.

The lab exists to answer one question:

> Does a branch-native persistent representation materially improve the storage and access economics of branch-heavy agent state compared with simple content sharing?

## Phase 1

The first experiment compares two deliberately different adversaries under the same deterministic branch/edit/read plan:

- **persistent AVL rope** — immutable `Arc` nodes, path-copying split/join, AVL rebalancing, chunked leaves, direct range traversal;
- **windowed CDC dedup** — a fixed-window rolling buzhash-style content-defined chunker, exact-byte collision verification, deduplicated payload storage, and per-version manifests with binary-searched range reads. Boundary fingerprints are not reset at chunk boundaries, so insert/delete edits can resynchronize after a local window instead of perturbing the chunk phase through a large suffix. Edits still reconstruct and re-chunk the parent state, making this a storage/locality adversary rather than an optimized dynamic CDC implementation.

Every generated child chooses an arbitrary historical parent. Both backends must retain all version handles. Sampled versions are fully decoded and compared byte-for-byte after timing.

The repository intentionally excludes durability/WAL, networking, auth, crypto, framework integrations, production APIs, and Lean↔Rust refinement. Those only become relevant if a representation survives the benchmark.

### Quarantined pre-windowed results

Results produced before commit `2f4147cdc95b6e3347b10983a9f846990b7d2684` used a weaker chunk-prefix fingerprint that reset at every chunk boundary. Those runs are useful as implementation diagnostics but **must not be used as evidence that AVL beats a strong CDC baseline**, because a small insertion/deletion could perturb boundary phase far into the suffix. Re-run the same workload at or after that commit for the novelty decision.

## What is measured

The harness reports:

- build time;
- edit latency p50/p95/p99;
- range-read latency p50/p95/p99;
- retained payload bytes;
- retained explicit metadata bytes;
- retained growth per branch;
- lifetime **representation** allocation per branch;
- live versus lifetime-allocated structural objects;
- a read checksum plus sampled cross-backend semantic equality.

Storage numbers are **estimates**, not RSS or filesystem bytes. They count payload plus explicit nodes/manifests. Allocator control blocks, hash-table bucket capacity, process/runtime overhead, and temporary buffers used while reading/re-chunking are excluded. The lifetime-allocation metric therefore measures allocations retained by or created for the representation itself; it is not a full heap-allocation or physical-write-amplification measurement. This limitation applies to both backends and is printed by the benchmark.

## Kill rule

A representation is not interesting merely because it is elegant, persistent, compressed, or formally provable.

Do not promote the AVL design into a production engine merely because it beats the deliberately non-incremental CDC implementation on edit CPU. The useful signal is a **large combined advantage** on branch-heavy workloads after retained metadata, representation allocation, historical range reads, and realistic state shapes are counted.

If the AVL rope is close to windowed CDC on retained growth, or if its storage win is purchased with materially worse reads/metadata, it is only a baseline. If a later incremental CDC/COW implementation comes within roughly 20–25% of the winning design on the intended workload with much less complexity, prefer the simpler design.

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

The windowed CDC backend currently scans/re-chunks the full parent on each edit, so the 10k × 8 MiB run is intentionally expensive. Do not interpret that CPU gap as product novelty; retained growth and read behavior are the more useful Phase-1 signals for deciding what to prototype next.

Use `cargo run --release -- --help` for all options.

## Scope discipline

No server. No CLI product. No database. No WAL. No crypto. No agent SDK. No production persistence. No benchmark-specific shortcuts in a backend.

If a feature does not help falsify or compare state-representation choices, it stays out of this repository.
