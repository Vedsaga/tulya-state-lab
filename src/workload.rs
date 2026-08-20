use crate::model::{Backend, BackendStats, Edit, StateError};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct Config {
    pub branches: usize,
    pub base_bytes: usize,
    pub max_edit_bytes: usize,
    pub read_bytes: usize,
    pub verify_samples: usize,
    pub seed: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            branches: 1_000,
            base_bytes: 2 * 1024 * 1024,
            max_edit_bytes: 96,
            read_bytes: 4096,
            verify_samples: 16,
            seed: 0x5eed_1234_d15c_a11e,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlannedOp {
    pub parent: usize,
    pub edit: Edit,
    pub read_start: usize,
    pub read_len: usize,
}

#[derive(Clone, Debug)]
pub struct Workload {
    pub config: Config,
    pub base: Vec<u8>,
    pub ops: Vec<PlannedOp>,
    pub version_lengths: Vec<usize>,
    pub logical_version_bytes: u128,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LatencySummary {
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub backend: &'static str,
    pub build_ns: u64,
    pub edit: LatencySummary,
    pub read: LatencySummary,
    pub initial_stats: BackendStats,
    pub final_stats: BackendStats,
    pub checksum: u64,
    pub logical_version_bytes: u128,
}

pub struct Run<B: Backend> {
    pub backend: B,
    pub snapshots: Vec<B::Snapshot>,
    pub report: Report,
}

#[derive(Clone, Copy)]
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn usize(&mut self, upper_exclusive: usize) -> usize {
        if upper_exclusive == 0 {
            0
        } else {
            (self.next_u64() as usize) % upper_exclusive
        }
    }
}

fn structured_base(len: usize, seed: u64) -> Vec<u8> {
    const TEMPLATE: &[u8] = br#"{"role":"assistant","tool":"state","status":"ok","content":"persistent branch data"}\n"#;
    let mut out = vec![0u8; len];
    let mut noise = Rng(seed ^ 0xa5a5_5a5a_c3c3_3c3c);
    for (i, byte) in out.iter_mut().enumerate() {
        let within_page = i % 4096;
        if within_page < 64 {
            let page = i / 4096;
            let shift = (within_page % usize::BITS as usize) as u32;
            *byte = ((page.rotate_left(shift) ^ within_page) & 0xff) as u8;
        } else if within_page < 3072 {
            *byte = TEMPLATE[i % TEMPLATE.len()];
        } else {
            *byte = (noise.next_u64() & 0xff) as u8;
        }
    }
    out
}

fn edit_payload(rng: &mut Rng, len: usize) -> Vec<u8> {
    const ALPHABET: &[u8] = b"{\"role\":\"assistant\",\"tool_call\":true,\"result\":\"ok\"}\n0123456789abcdef";
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let jitter = rng.usize(ALPHABET.len());
        out.push(ALPHABET[(i + jitter) % ALPHABET.len()]);
    }
    out
}

impl Workload {
    pub fn generate(config: Config) -> Self {
        let base = structured_base(config.base_bytes, config.seed);
        let mut rng = Rng(config.seed);
        let mut version_lengths = Vec::with_capacity(config.branches + 1);
        version_lengths.push(base.len());
        let mut logical_version_bytes = base.len() as u128;
        let mut ops = Vec::with_capacity(config.branches);
        let max_edit = config.max_edit_bytes.max(1);

        for _ in 0..config.branches {
            let parent = rng.usize(version_lengths.len());
            let parent_len = version_lengths[parent];
            let start = rng.usize(parent_len.saturating_add(1));
            let available = parent_len - start;
            let mode = rng.usize(10);

            let (delete_len, insert_len) = match mode {
                0..=5 => {
                    let n = (1 + rng.usize(max_edit)).min(available);
                    (n, n)
                }
                6..=7 => (0, 1 + rng.usize(max_edit)),
                _ => ((1 + rng.usize(max_edit)).min(available), 0),
            };
            let insert = edit_payload(&mut rng, insert_len);
            let edit = Edit {
                start,
                delete_len,
                insert,
            };
            let child_len = edit.output_len(parent_len).expect("generated edit is valid");
            let read_len = config.read_bytes.min(child_len);
            let read_start = if read_len == 0 {
                0
            } else {
                rng.usize(child_len - read_len + 1)
            };
            ops.push(PlannedOp {
                parent,
                edit,
                read_start,
                read_len,
            });
            version_lengths.push(child_len);
            logical_version_bytes = logical_version_bytes.saturating_add(child_len as u128);
        }

        Self {
            config,
            base,
            ops,
            version_lengths,
            logical_version_bytes,
        }
    }
}

fn percentile(values: &[u64], numerator: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let idx = (sorted.len() - 1).saturating_mul(numerator) / 100;
    sorted[idx]
}

fn summarize(values: &[u64]) -> LatencySummary {
    LatencySummary {
        p50_ns: percentile(values, 50),
        p95_ns: percentile(values, 95),
        p99_ns: percentile(values, 99),
    }
}

fn checksum_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub fn run_backend<B: Backend>(mut backend: B, workload: &Workload) -> Result<Run<B>, StateError> {
    let build_start = Instant::now();
    let base = backend.create(&workload.base);
    let build_ns = build_start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    let initial_stats = backend.stats();

    let mut snapshots = Vec::with_capacity(workload.ops.len() + 1);
    snapshots.push(base);
    let mut edit_latencies = Vec::with_capacity(workload.ops.len());
    let mut read_latencies = Vec::with_capacity(workload.ops.len());
    let mut checksum = 0xcbf2_9ce4_8422_2325u64;

    for op in &workload.ops {
        let parent = &snapshots[op.parent];
        let edit_start = Instant::now();
        let child = backend.edit(parent, &op.edit)?;
        edit_latencies.push(edit_start.elapsed().as_nanos().min(u64::MAX as u128) as u64);

        let read_start = Instant::now();
        let bytes = backend.read_range(&child, op.read_start, op.read_len)?;
        read_latencies.push(read_start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        checksum = checksum_bytes(checksum, &bytes);
        snapshots.push(child);
    }

    let final_stats = backend.stats();
    let report = Report {
        backend: backend.name(),
        build_ns,
        edit: summarize(&edit_latencies),
        read: summarize(&read_latencies),
        initial_stats,
        final_stats,
        checksum,
        logical_version_bytes: workload.logical_version_bytes,
    };

    Ok(Run {
        backend,
        snapshots,
        report,
    })
}

pub fn verify_pair<A: Backend, B: Backend>(
    left: &Run<A>,
    right: &Run<B>,
    workload: &Workload,
) -> Result<(), String> {
    if left.snapshots.len() != right.snapshots.len() {
        return Err("backend snapshot counts differ".into());
    }
    if left.snapshots.len() != workload.version_lengths.len() {
        return Err("snapshot count does not match workload plan".into());
    }

    for (i, expected_len) in workload.version_lengths.iter().copied().enumerate() {
        let left_len = left.backend.len(&left.snapshots[i]);
        let right_len = right.backend.len(&right.snapshots[i]);
        if left_len != expected_len || right_len != expected_len {
            return Err(format!(
                "version {i} length mismatch: expected={expected_len}, left={left_len}, right={right_len}"
            ));
        }
    }

    let count = workload.config.verify_samples.max(2).min(left.snapshots.len());
    for sample in 0..count {
        let index = if sample + 1 == count {
            left.snapshots.len() - 1
        } else {
            sample.saturating_mul(left.snapshots.len() - 1) / (count - 1)
        };
        left.backend.validate(&left.snapshots[index])?;
        right.backend.validate(&right.snapshots[index])?;
        let left_bytes = left
            .backend
            .read_all(&left.snapshots[index])
            .map_err(|e| e.to_string())?;
        let right_bytes = right
            .backend
            .read_all(&right.snapshots[index])
            .map_err(|e| e.to_string())?;
        if left_bytes != right_bytes {
            return Err(format!("semantic mismatch at sampled version {index}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workload_parents_and_edits_are_valid() {
        let workload = Workload::generate(Config {
            branches: 500,
            base_bytes: 32 * 1024,
            max_edit_bytes: 64,
            read_bytes: 1024,
            verify_samples: 8,
            seed: 7,
        });
        assert_eq!(workload.version_lengths.len(), 501);
        for (i, op) in workload.ops.iter().enumerate() {
            assert!(op.parent <= i);
            op.edit
                .validate_len(workload.version_lengths[op.parent])
                .unwrap();
            assert_eq!(
                op.edit
                    .output_len(workload.version_lengths[op.parent])
                    .unwrap(),
                workload.version_lengths[i + 1]
            );
        }
    }
}
