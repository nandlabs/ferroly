//! The parser budget shared by every codec, plus the small helpers the parsers
//! use to enforce it and to report error positions.
//!
//! All four parsers (JSON, XML, YAML, TOML) draw from one [`Limits`] value: a
//! maximum nesting depth, a maximum input size, and a maximum number of entries
//! (array elements plus object members). The simple entry points
//! (`json::decode`, `yaml::from_str`, …) use [`Limits::default`]; the `*_with`
//! variants take an explicit budget.

use std::collections::HashMap;
use std::fmt;

use ferroly::codec::CodecError;

/// Resource limits applied while parsing untrusted input.
///
/// ```
/// use ferroly::codec::{json, CodecError, Limits};
///
/// let limits = Limits::new().with_max_depth(4).with_max_entries(100);
/// assert!(json::from_str_with("[[1, 2], [3]]", limits).is_ok());
///
/// let err = json::from_str_with("[[[[[1]]]]]", limits).unwrap_err();
/// assert!(matches!(err, CodecError::LimitExceeded(_)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Maximum container nesting depth. The parsers are recursive, so this also
    /// bounds stack usage; raising it far beyond the default risks exhausting
    /// the stack on hostile input.
    pub max_depth: usize,
    /// Maximum input size in bytes; larger inputs are rejected before parsing.
    pub max_input_bytes: usize,
    /// Maximum total number of entries — array elements plus object members
    /// (XML: elements; TOML: keys, tables, and array elements) — in one
    /// document. This bounds the memory a small input can expand into.
    pub max_entries: usize,
}

impl Limits {
    /// The default nesting depth (128 — also `serde_json`'s recursion limit).
    pub const DEFAULT_MAX_DEPTH: usize = 128;
    /// The default input size cap: 64 MiB.
    pub const DEFAULT_MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
    /// The default entry cap: ten million entries.
    pub const DEFAULT_MAX_ENTRIES: usize = 10_000_000;

    /// The default budget, usable in `const` contexts.
    pub const DEFAULT: Limits = Limits {
        max_depth: Self::DEFAULT_MAX_DEPTH,
        max_input_bytes: Self::DEFAULT_MAX_INPUT_BYTES,
        max_entries: Self::DEFAULT_MAX_ENTRIES,
    };

    /// The default budget (same as [`Limits::default`]).
    pub const fn new() -> Self {
        Self::DEFAULT
    }

    /// Returns the budget with a different nesting-depth cap.
    #[must_use]
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Returns the budget with a different input-size cap (bytes).
    #[must_use]
    pub fn with_max_input_bytes(mut self, max_input_bytes: usize) -> Self {
        self.max_input_bytes = max_input_bytes;
        self
    }

    /// Returns the budget with a different entry-count cap.
    #[must_use]
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The running counters a parser checks against its [`Limits`].
pub(crate) struct Budget {
    max_depth: usize,
    max_entries: usize,
    depth: usize,
    entries: usize,
}

impl Budget {
    /// Starts a budget for an input of `input_len` bytes, rejecting it outright
    /// when it exceeds the size cap.
    pub(crate) fn new(limits: Limits, input_len: usize) -> Result<Self, CodecError> {
        if input_len > limits.max_input_bytes {
            return Err(CodecError::LimitExceeded(format!(
                "input is {input_len} bytes, over the limit of {}",
                limits.max_input_bytes
            )));
        }
        Ok(Budget {
            max_depth: limits.max_depth,
            max_entries: limits.max_entries,
            depth: 0,
            entries: 0,
        })
    }

    /// The configured nesting-depth cap.
    pub(crate) fn max_depth(&self) -> usize {
        self.max_depth
    }

    /// Enters a nested container. Paired with [`leave`](Self::leave) on every
    /// non-error return.
    #[inline]
    pub(crate) fn enter(&mut self) -> Result<(), CodecError> {
        self.depth += 1;
        if self.depth > self.max_depth {
            return Err(depth_error(self.max_depth));
        }
        Ok(())
    }

    /// Leaves the container entered by the matching [`enter`](Self::enter).
    #[inline]
    pub(crate) fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Counts one array element / object member against the entry cap.
    #[inline]
    pub(crate) fn entry(&mut self) -> Result<(), CodecError> {
        self.entries += 1;
        if self.entries > self.max_entries {
            return Err(CodecError::LimitExceeded(format!(
                "document has more than {} entries",
                self.max_entries
            )));
        }
        Ok(())
    }
}

/// The error for input nested deeper than `max_depth`.
pub(crate) fn depth_error(max_depth: usize) -> CodecError {
    CodecError::LimitExceeded(format!("nesting is deeper than {max_depth} levels"))
}

/// Converts a byte offset into a 1-based `(line, column)` pair. Columns count
/// characters, not bytes. Only called on the error path, so the linear scan is
/// free for well-formed input.
pub(crate) fn line_col(input: &str, pos: usize) -> (usize, usize) {
    let bytes = input.as_bytes();
    let pos = pos.min(bytes.len());
    let mut line = 1;
    let mut line_start = 0;
    for (i, &b) in bytes[..pos].iter().enumerate() {
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    // Count characters (bytes that are not UTF-8 continuation bytes).
    let column = bytes[line_start..pos]
        .iter()
        .filter(|&&b| (b & 0xC0) != 0x80)
        .count()
        + 1;
    (line, column)
}

/// Builds a parse error carrying the line and column of byte offset `pos`.
pub(crate) fn error_at(input: &str, pos: usize, msg: impl fmt::Display) -> CodecError {
    let (line, column) = line_col(input, pos);
    CodecError::Parse(format!("{msg} at line {line}, column {column}"))
}

/// Above this many keys an object under construction switches from a linear
/// key scan to a hash index. Small objects (the common case) never pay for the
/// map; large ones stay linear overall instead of quadratic.
const INDEX_THRESHOLD: usize = 16;

/// A lazily-built key → position index over an entry list that a parser is
/// appending to. The caller owns the entries; this only accelerates lookup.
#[derive(Default)]
pub(crate) struct KeyIndex {
    map: Option<HashMap<String, usize>>,
}

impl KeyIndex {
    /// Finds the position of `key` in `entries` (keys must be unique).
    pub(crate) fn find<V>(&mut self, entries: &[(String, V)], key: &str) -> Option<usize> {
        if self.map.is_none() {
            if entries.len() <= INDEX_THRESHOLD {
                return entries.iter().position(|(k, _)| k == key);
            }
            self.map = Some(
                entries
                    .iter()
                    .enumerate()
                    .map(|(i, (k, _))| (k.clone(), i))
                    .collect(),
            );
        }
        self.map.as_ref().and_then(|m| m.get(key).copied())
    }

    /// Records that `key` was just appended at position `idx`.
    pub(crate) fn inserted(&mut self, key: &str, idx: usize) {
        if let Some(map) = &mut self.map {
            map.insert(key.to_string(), idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_counts_lines_and_characters() {
        let s = "ab\ncé d\n";
        assert_eq!(line_col(s, 0), (1, 1));
        assert_eq!(line_col(s, 2), (1, 3));
        assert_eq!(line_col(s, 3), (2, 1));
        // 'é' is two bytes; the space after it is column 3.
        assert_eq!(line_col(s, 6), (2, 3));
        assert_eq!(line_col(s, 999), (3, 1));
    }

    #[test]
    fn budget_enforces_each_limit() {
        let limits = Limits::new()
            .with_max_depth(2)
            .with_max_entries(3)
            .with_max_input_bytes(10);
        assert!(Budget::new(limits, 11).is_err());
        let mut b = Budget::new(limits, 10).unwrap();
        b.enter().unwrap();
        b.enter().unwrap();
        assert!(b.enter().is_err());
        let mut b = Budget::new(limits, 0).unwrap();
        for _ in 0..3 {
            b.entry().unwrap();
        }
        assert!(b.entry().is_err());
    }

    #[test]
    fn key_index_agrees_with_linear_scan() {
        let mut entries: Vec<(String, u32)> = Vec::new();
        let mut index = KeyIndex::default();
        for i in 0..100u32 {
            let key = format!("k{i}");
            assert_eq!(index.find(&entries, &key), None);
            entries.push((key.clone(), i));
            index.inserted(&key, entries.len() - 1);
        }
        for i in 0..100usize {
            assert_eq!(index.find(&entries, &format!("k{i}")), Some(i));
        }
        assert_eq!(index.find(&entries, "missing"), None);
    }
}
