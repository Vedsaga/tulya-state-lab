use std::error::Error;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub delete_len: usize,
    pub insert: Vec<u8>,
}

impl Edit {
    pub fn validate_len(&self, input_len: usize) -> Result<(), StateError> {
        let end = self
            .start
            .checked_add(self.delete_len)
            .ok_or(StateError::LengthOverflow)?;
        if self.start > input_len || end > input_len {
            return Err(StateError::OutOfBounds {
                len: input_len,
                start: self.start,
                delete_len: self.delete_len,
            });
        }
        Ok(())
    }

    pub fn output_len(&self, input_len: usize) -> Result<usize, StateError> {
        self.validate_len(input_len)?;
        input_len
            .checked_sub(self.delete_len)
            .and_then(|n| n.checked_add(self.insert.len()))
            .ok_or(StateError::LengthOverflow)
    }

    pub fn apply(&self, input: &[u8]) -> Result<Vec<u8>, StateError> {
        self.validate_len(input.len())?;
        let output_len = self.output_len(input.len())?;
        let end = self.start + self.delete_len;
        let mut out = Vec::with_capacity(output_len);
        out.extend_from_slice(&input[..self.start]);
        out.extend_from_slice(&self.insert);
        out.extend_from_slice(&input[end..]);
        Ok(out)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateError {
    OutOfBounds {
        len: usize,
        start: usize,
        delete_len: usize,
    },
    LengthOverflow,
    Corrupt(&'static str),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateError::OutOfBounds {
                len,
                start,
                delete_len,
            } => write!(
                f,
                "edit/range out of bounds: len={len}, start={start}, delete_len={delete_len}"
            ),
            StateError::LengthOverflow => write!(f, "state length arithmetic overflow"),
            StateError::Corrupt(msg) => write!(f, "backend invariant failure: {msg}"),
        }
    }
}

impl Error for StateError {}

#[derive(Clone, Copy, Debug, Default)]
pub struct BackendStats {
    pub retained_payload_bytes: usize,
    pub retained_metadata_bytes: usize,
    pub lifetime_payload_bytes: usize,
    pub lifetime_metadata_bytes: usize,
    pub live_objects: usize,
    pub total_objects_allocated: usize,
}

impl BackendStats {
    pub fn retained_bytes(self) -> usize {
        self.retained_payload_bytes
            .saturating_add(self.retained_metadata_bytes)
    }

    pub fn lifetime_allocated_bytes(self) -> usize {
        self.lifetime_payload_bytes
            .saturating_add(self.lifetime_metadata_bytes)
    }
}

pub trait Backend {
    type Snapshot: Clone;

    fn name(&self) -> &'static str;
    fn create(&mut self, bytes: &[u8]) -> Self::Snapshot;
    fn len(&self, snapshot: &Self::Snapshot) -> usize;

    fn read_range(
        &self,
        snapshot: &Self::Snapshot,
        start: usize,
        len: usize,
    ) -> Result<Vec<u8>, StateError>;

    fn edit(
        &mut self,
        parent: &Self::Snapshot,
        edit: &Edit,
    ) -> Result<Self::Snapshot, StateError>;

    fn validate(&self, snapshot: &Self::Snapshot) -> Result<(), String>;
    fn stats(&self) -> BackendStats;

    fn read_all(&self, snapshot: &Self::Snapshot) -> Result<Vec<u8>, StateError> {
        self.read_range(snapshot, 0, self.len(snapshot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_apply_matches_insert_delete_replace() {
        let base = b"abcdefghij";

        let insert = Edit {
            start: 3,
            delete_len: 0,
            insert: b"XYZ".to_vec(),
        };
        assert_eq!(insert.apply(base).unwrap(), b"abcXYZdefghij");

        let delete = Edit {
            start: 2,
            delete_len: 4,
            insert: vec![],
        };
        assert_eq!(delete.apply(base).unwrap(), b"abghij");

        let replace = Edit {
            start: 2,
            delete_len: 4,
            insert: b"Q".to_vec(),
        };
        assert_eq!(replace.apply(base).unwrap(), b"abQghij");
    }

    #[test]
    fn edit_rejects_invalid_ranges() {
        let edit = Edit {
            start: 5,
            delete_len: 2,
            insert: vec![],
        };
        assert!(matches!(
            edit.apply(b"abc"),
            Err(StateError::OutOfBounds { .. })
        ));
    }
}
