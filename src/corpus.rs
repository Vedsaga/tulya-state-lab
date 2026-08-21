use crate::model::{Backend, BackendStats, Edit, StateError};
use crate::workload::LatencySummary;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct CorpusCase {
    pub id: String,
    pub base: Vec<u8>,
    pub child_len: usize,
    pub edit: Edit,
    pub read_start: usize,
    pub read_len: usize,
}

#[derive(Clone, Debug)]
pub struct Corpus {
    pub cases: Vec<CorpusCase>,
    pub logical_bytes: u128,
}

#[derive(Clone, Debug)]
pub struct CorpusReport {
    pub backend: &'static str,
    pub base_build_ns: u64,
    pub edit: LatencySummary,
    pub read: LatencySummary,
    pub base_stats: BackendStats,
    pub final_stats: BackendStats,
    pub checksum: u64,
    pub cases: usize,
    pub logical_bytes: u128,
}

#[derive(Clone, Debug)]
pub struct CorpusOutcome {
    pub report: CorpusReport,
    pub sample_indices: Vec<usize>,
    pub sample_children: Vec<Vec<u8>>,
}

fn derive_edit(base: &[u8], child: &[u8]) -> Edit {
    let common_limit = base.len().min(child.len());
    let mut prefix = 0usize;
    while prefix < common_limit && base[prefix] == child[prefix] {
        prefix += 1;
    }

    let mut suffix = 0usize;
    let base_remaining = base.len().saturating_sub(prefix);
    let child_remaining = child.len().saturating_sub(prefix);
    let suffix_limit = base_remaining.min(child_remaining);
    while suffix < suffix_limit
        && base[base.len() - 1 - suffix] == child[child.len() - 1 - suffix]
    {
        suffix += 1;
    }

    Edit {
        start: prefix,
        delete_len: base.len() - prefix - suffix,
        insert: child[prefix..child.len() - suffix].to_vec(),
    }
}

fn checksum_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn id_hash(id: &str) -> u64 {
    checksum_bytes(0xcbf2_9ce4_8422_2325u64, id.as_bytes())
}

fn resolve_path(root: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn sample_indices(total: usize, verify_samples: usize) -> Vec<usize> {
    let count = verify_samples.max(2).min(total);
    (0..count)
        .map(|sample| {
            if sample + 1 == count {
                total - 1
            } else {
                sample.saturating_mul(total - 1) / (count - 1)
            }
        })
        .collect()
}

impl Corpus {
    /// Load a tab-separated manifest with at least three columns:
    ///
    /// `case_id<TAB>base_snapshot_path<TAB>child_snapshot_path`
    ///
    /// Extra columns are ignored so dataset metadata can travel with the trace.
    pub fn load_manifest(path: &Path, read_bytes: usize) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("failed to read corpus manifest {}: {e}", path.display()))?;
        let root = path.parent().unwrap_or_else(|| Path::new("."));
        let mut cases = Vec::new();
        let mut logical_bytes = 0u128;

        for (line_no, line) in text.lines().enumerate() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 3 {
                return Err(format!(
                    "manifest line {} must contain case_id, base path, and child path",
                    line_no + 1
                ));
            }
            let id = fields[0].to_string();
            let base_path = resolve_path(root, fields[1]);
            let child_path = resolve_path(root, fields[2]);
            let base = fs::read(&base_path).map_err(|e| {
                format!(
                    "failed to read base snapshot for {id} ({}): {e}",
                    base_path.display()
                )
            })?;
            let child = fs::read(&child_path).map_err(|e| {
                format!(
                    "failed to read child snapshot for {id} ({}): {e}",
                    child_path.display()
                )
            })?;
            let child_len = child.len();
            let edit = derive_edit(&base, &child);
            let rebuilt = edit
                .apply(&base)
                .map_err(|e| format!("derived edit for {id} is invalid: {e}"))?;
            if rebuilt != child {
                return Err(format!("derived edit for {id} does not reconstruct child"));
            }

            let read_len = read_bytes.min(child_len);
            let read_start = if read_len == 0 {
                0
            } else {
                (id_hash(&id) as usize) % (child_len - read_len + 1)
            };
            logical_bytes = logical_bytes
                .saturating_add(base.len() as u128)
                .saturating_add(child_len as u128);
            cases.push(CorpusCase {
                id,
                base,
                child_len,
                edit,
                read_start,
                read_len,
            });
        }

        if cases.is_empty() {
            return Err("corpus manifest contains no cases".into());
        }
        Ok(Self {
            cases,
            logical_bytes,
        })
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

pub fn run_corpus_backend<B: Backend>(
    mut backend: B,
    corpus: &Corpus,
    verify_samples: usize,
) -> Result<CorpusOutcome, StateError> {
    let base_build_start = Instant::now();
    let mut bases = Vec::with_capacity(corpus.cases.len());
    for case in &corpus.cases {
        bases.push(backend.create(&case.base));
    }
    let base_build_ns = base_build_start
        .elapsed()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    let base_stats = backend.stats();

    let mut children = Vec::with_capacity(corpus.cases.len());
    let mut edit_latencies = Vec::with_capacity(corpus.cases.len());
    let mut read_latencies = Vec::with_capacity(corpus.cases.len());
    let mut checksum = 0xcbf2_9ce4_8422_2325u64;

    for (case, base) in corpus.cases.iter().zip(&bases) {
        let edit_start = Instant::now();
        let child = backend.edit(base, &case.edit)?;
        edit_latencies.push(edit_start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        if backend.len(&child) != case.child_len {
            return Err(StateError::Corrupt("corpus edit produced wrong child length"));
        }

        let read_start = Instant::now();
        let bytes = backend.read_range(&child, case.read_start, case.read_len)?;
        read_latencies.push(read_start.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        checksum = checksum_bytes(checksum, &bytes);
        children.push(child);
    }

    let final_stats = backend.stats();
    let selected = sample_indices(corpus.cases.len(), verify_samples);
    let mut sample_children = Vec::with_capacity(selected.len());
    for &index in &selected {
        backend
            .validate(&children[index])
            .map_err(|_| StateError::Corrupt("corpus backend validation failed"))?;
        sample_children.push(backend.read_all(&children[index])?);
    }

    let report = CorpusReport {
        backend: backend.name(),
        base_build_ns,
        edit: summarize(&edit_latencies),
        read: summarize(&read_latencies),
        base_stats,
        final_stats,
        checksum,
        cases: corpus.cases.len(),
        logical_bytes: corpus.logical_bytes,
    };
    Ok(CorpusOutcome {
        report,
        sample_indices: selected,
        sample_children,
    })
}

pub fn verify_corpus_outcomes(
    left: &CorpusOutcome,
    right: &CorpusOutcome,
    corpus: &Corpus,
) -> Result<(), String> {
    if left.sample_indices != right.sample_indices
        || left.sample_children.len() != left.sample_indices.len()
        || right.sample_children.len() != right.sample_indices.len()
    {
        return Err("corpus verification sample sets differ".into());
    }

    for (sample_pos, &index) in left.sample_indices.iter().enumerate() {
        let case = &corpus.cases[index];
        let expected = case
            .edit
            .apply(&case.base)
            .map_err(|e| format!("failed to reconstruct expected child {}: {e}", case.id))?;
        let left_bytes = &left.sample_children[sample_pos];
        let right_bytes = &right.sample_children[sample_pos];
        if left_bytes != &expected || right_bytes != &expected || left_bytes != right_bytes {
            return Err(format!("corpus semantic mismatch at case {}", case.id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contiguous_edit_reconstructs_arbitrary_pair() {
        let base = b"alpha-OLD-middle-tail";
        let child = b"alpha-NEW-and-longer-middle-tail";
        let edit = derive_edit(base, child);
        assert_eq!(edit.apply(base).unwrap(), child);
    }

    #[test]
    fn contiguous_edit_handles_identical_inputs() {
        let bytes = b"same bytes";
        let edit = derive_edit(bytes, bytes);
        assert_eq!(edit.delete_len, 0);
        assert!(edit.insert.is_empty());
        assert_eq!(edit.apply(bytes).unwrap(), bytes);
    }
}
