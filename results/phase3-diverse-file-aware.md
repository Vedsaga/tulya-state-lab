# Phase 3 corrected diverse real-corpus result

Status: **accepted as the corrected 24-case real-corpus gate**.

This result supersedes the quarantined single-span diverse-corpus interpretation. The prepared `swebench-verified-diverse` corpus was replayed with file-aware exact edit scripts at commit `9f89f83b9e909662f28a75b330221cd8a8a4ad76`.

## Corpus

- 24 repository-diverse SWE-bench Verified cases
- 924,010,567 logical base+child bytes (0.861 GiB)
- 33 exact edit hunks total
- 1.38 hunks per case
- sampled semantic cross-check: PASS across all three backends

## Corrected result

| backend | retained bases | child growth / case | edit p95 | read p95 |
| --- | ---: | ---: | ---: | ---: |
| persistent AVL byte rope | 449.205 MiB | 56,191.8 B | **79.331 us** | 3.307 us |
| persistent COW + exact interning | 450.926 MiB | **51,802.5 B** | 133.826 us | 1.854 us |
| incremental windowed CDC | **252.086 MiB** | 79,678.2 B | 955.296 us | **1.603 us** |

Pairwise COW vs CDC:

- child retained growth: 1,243,259 vs 1,912,277 bytes; COW uses about 35% less
- edit p95: 133.826 us vs 955.296 us; COW is about 7.1x faster
- read p95: 1.854 us vs 1.603 us; CDC is about 16% faster
- retained independent bases: 450.926 MiB vs 252.086 MiB; CDC uses about 44% less

## Interpretation

The previous ~28.5x CDC child-growth advantage was an artifact of collapsing distant repository changes into one large contiguous replacement. Once the same real snapshots are replayed with localized exact file-aware edits, COW again wins the primary branch/checkpoint metric: **new retained bytes per child**.

This does **not** erase CDC's separate cold/global-storage result. CDC materially deduplicates content across independent base snapshots, while the current COW importer stores each independent base largely as its own backing buffer. Keep these two questions separate:

1. branch/checkpoint growth from an existing historical parent: COW currently wins;
2. global storage of many independent but overlapping roots: CDC currently wins.

The current CDC strong chunk pool also retains transient chunks from intermediate multi-hunk stages, so a narrow CDC child-storage loss would require reclamation work before rejection. This corrected loss is not narrow enough to change the branch/checkpoint conclusion, but the caveat remains relevant for later CDC comparisons.

## Decision

- Do not resume AVL work.
- Do not implement grammar/recompression from this result.
- Keep persistent COW + exact immutable-buffer interning as the representation baseline for branch/checkpoint growth.
- Keep CDC as the adversary for global cross-root deduplication.
- The next product gate is an external framework baseline, not another custom representation: pin and benchmark current LangGraph Postgres checkpointing / DeltaChannel behavior under branch-heavy workloads, while recording that DeltaChannel is still beta and that current Postgres copy/prune/historical-branch behavior has active limitations.
