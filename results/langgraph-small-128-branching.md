# Phase 4 — LangGraph/PostgresSaver 128-branch smoke

Validated locally at repository head `d35905fe2d2e7b4196d19c36fa7860792bd131df` with:

- `langgraph==1.2.10`
- `langgraph-checkpoint==4.2.0`
- `langgraph-checkpoint-postgres==3.1.1`
- `psycopg==3.3.4`
- workload `small-edit`
- 128 child versions
- 256 KiB base
- max edit 96 bytes
- seed `0x5eed1234d15ca11e`
- DeltaChannel snapshot frequency 1000

## Plain PostgresSaver

Semantic verification: **PASS**.

| metric | result |
| --- | ---: |
| base PostgreSQL relations | 0.258 MiB |
| final PostgreSQL relations | 10.820 MiB |
| physical relation growth / branch | 86,528 B |
| `update_state` p95 | 5,747.488 us |
| historical `get_state` + 4 KiB slice p95 | 1,908.839 us |
| reopen historical `get_state` p95 | 1,847.058 us |
| checkpoints | 129 |
| checkpoint blobs | 129 |
| checkpoint writes | 67 |

This is a valid baseline for the arbitrary-historical-parent workload, but it is much heavier than the in-process persistent COW representation measured in Phase 2. Do not compare the latencies as if they were equivalent deployment boundaries: Postgres includes database/process/serialization overhead, while the Rust lab backend is in-process representation cost. Storage growth remains a meaningful system-level comparison.

## DeltaChannel

Semantic verification: **FAIL** at version 3.

The deterministic workload begins:

```text
v1 <- v0
v2 <- v1
v3 <- v0
```

The first historical fork is therefore version 3, and that is exactly where the reconstructed state length diverges.

Partial database state at failure:

| metric | result |
| --- | ---: |
| base PostgreSQL relations | 0.258 MiB |
| relations at failure | 0.281 MiB |
| checkpoint rows | 4 |
| checkpoint blobs | 1 |
| checkpoint writes | 2 |

Do **not** interpret the apparent 192 B/branch growth or partial latencies as a storage/performance result. The run stopped after only two successful child writes.

This behavior matches the currently open LangGraph DeltaChannel historical-fork bug: writes attached to a shared parent do not encode which child consumed them, so replay on a fork can include writes from the abandoned sibling branch. The failure therefore disqualifies the current stable DeltaChannel as a branch-native product baseline for arbitrary historical-parent execution.

Next diagnostic: run `DeltaChannel` only on `linear-append`. That run is a supported-shape/storage diagnostic, not a substitute for the failed branch-native gate.
