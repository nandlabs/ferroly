//! The decode budget applied to untrusted protobuf input.

/// Resource limits applied while decoding a message.
///
/// ```
/// use ferroly::proto::{Limits, Message, ProtoError};
///
/// #[derive(ferroly::proto::Message, Default, Debug)]
/// struct Node {
///     #[proto(tag = 1)]
///     child: Option<Box<Node>>,
/// }
///
/// // Twenty levels of `child { child { … } }`.
/// let mut bytes = Vec::new();
/// for _ in 0..20 {
///     let mut outer = vec![0x0a, bytes.len() as u8];
///     outer.extend_from_slice(&bytes);
///     bytes = outer;
/// }
/// assert!(Node::decode(&bytes).is_ok());
/// let shallow = Limits::new().with_max_depth(8);
/// assert!(matches!(
///     Node::decode_with(&bytes, shallow),
///     Err(ProtoError::DepthExceeded(8))
/// ));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Maximum nesting depth of embedded messages, map entries and groups.
    /// Decoding recurses once per level, so this also bounds stack usage.
    pub max_depth: usize,
    /// Maximum input size in bytes; larger inputs are rejected up front.
    pub max_input_bytes: usize,
    /// Maximum number of fields plus packed elements in one input. This bounds
    /// the work a decode does; [`max_decoded_bytes`](Self::max_decoded_bytes)
    /// bounds its memory.
    pub max_entries: usize,
    /// Maximum estimated in-memory size, in bytes, of what one decode creates.
    ///
    /// Wire size says little about memory: an empty sub-message costs two
    /// bytes on the wire (`0a 00`) but a whole `T::default()` once decoded, so
    /// a 4 MiB input of them can expand into gigabytes. The decoder charges,
    /// before it allocates:
    ///
    /// - `size_of::<T>()` for each element of a repeated field, per slot of
    ///   the vector's growth (so the charge follows the real capacity);
    /// - twice the key and value sizes for each map entry (tree and hash
    ///   maps hold spare room);
    /// - the boxed size for each `Box<M>` created (an `Option<Box<M>>` field
    ///   or a boxed oneof variant, see
    ///   [`Message::DEFAULT_HEAP_BYTES`](super::Message::DEFAULT_HEAP_BYTES));
    /// - the bytes of each `string` / `bytes` value and of each unknown field,
    ///   plus the size of the unknown-field record.
    ///
    /// Fields held inline in a struct that already exists cost nothing extra.
    /// The estimate ignores allocator overhead, so real usage can be somewhat
    /// higher, but it grows with this limit, not with the input's shape.
    /// Exceeding it fails with [`ProtoError::LimitExceeded`](super::ProtoError::LimitExceeded).
    pub max_decoded_bytes: usize,
}

impl Limits {
    /// The default nesting depth (100, the limit the reference
    /// implementations use).
    pub const DEFAULT_MAX_DEPTH: usize = 100;
    /// The default input size cap: 64 MiB.
    pub const DEFAULT_MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
    /// The default entry cap: ten million.
    pub const DEFAULT_MAX_ENTRIES: usize = 10_000_000;
    /// The default decoded-size cap: 256 MiB, four times the default input
    /// cap. Services that know their message sizes should lower it (gRPC sets
    /// it from its receive-message limit).
    pub const DEFAULT_MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;

    /// The default budget, usable in `const` contexts.
    pub const DEFAULT: Limits = Limits {
        max_depth: Self::DEFAULT_MAX_DEPTH,
        max_input_bytes: Self::DEFAULT_MAX_INPUT_BYTES,
        max_entries: Self::DEFAULT_MAX_ENTRIES,
        max_decoded_bytes: Self::DEFAULT_MAX_DECODED_BYTES,
    };

    /// The default budget (same as [`Limits::default`]).
    pub const fn new() -> Self {
        Self::DEFAULT
    }

    /// Returns the budget with a different nesting-depth cap.
    ///
    /// Decoding recurses once per level, and so do encoding and dropping a
    /// deeply nested message. The default of 100 is safe on any thread; on
    /// the 2 MiB stack tokio and `std::thread` give a spawned thread, depths
    /// up to about 300 are safe in debug builds and about 1000 in release
    /// builds (the measured overflow points are two to three times that).
    /// Raise it
    /// beyond that only together with the stack size of the decoding thread
    /// (`std::thread::Builder::stack_size`, tokio's `thread_stack_size`).
    /// Messages built in memory are not depth-checked when encoded or dropped.
    #[must_use]
    pub const fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Returns the budget with a different input-size cap (bytes).
    #[must_use]
    pub const fn with_max_input_bytes(mut self, max_input_bytes: usize) -> Self {
        self.max_input_bytes = max_input_bytes;
        self
    }

    /// Returns the budget with a different entry-count cap.
    #[must_use]
    pub const fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }

    /// Returns the budget with a different cap on the estimated in-memory size
    /// of the decoded value (bytes); see
    /// [`max_decoded_bytes`](Self::max_decoded_bytes).
    #[must_use]
    pub const fn with_max_decoded_bytes(mut self, max_decoded_bytes: usize) -> Self {
        self.max_decoded_bytes = max_decoded_bytes;
        self
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}
