//! Document loaders: plain-text and Markdown files (or whole directories of
//! them) read into [`TextChunk`]s ready to embed and store.
//!
//! The loaders only read and cut text; they do not embed. Hand the chunks to
//! `genai::rag::RagIndex::upsert` (each converts into a `RagDocument`), or
//! embed them yourself and store [`Doc`](super::Doc)s.

use std::path::{Path, PathBuf};

use ferroly::codec::Value;

use super::chunk::Chunker;
use super::VectorStoreError;

/// The format of a loaded text, chosen from the file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextFormat {
    /// Plain text (`.txt`, `.text`, or anything not recognised as Markdown).
    Plain,
    /// Markdown (`.md`, `.markdown`). Chunks record the heading they fall
    /// under.
    Markdown,
}

impl TextFormat {
    /// The format for a path's extension (ignoring case); `None` for an
    /// extension the directory loader does not read.
    pub fn for_path(path: &Path) -> Option<TextFormat> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "md" | "markdown" => Some(TextFormat::Markdown),
            "txt" | "text" => Some(TextFormat::Plain),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            TextFormat::Plain => "text",
            TextFormat::Markdown => "markdown",
        }
    }
}

/// A whole loaded document.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedText {
    /// Where the text came from (the file path, or any caller-chosen id).
    /// Chunk ids are derived from it.
    pub source: String,
    /// The text's format.
    pub format: TextFormat,
    /// The full text.
    pub text: String,
}

/// One chunk of a [`LoadedText`], ready to embed.
#[derive(Debug, Clone, PartialEq)]
pub struct TextChunk {
    /// `"<source>#<index>"`, stable for an unchanged file.
    pub id: String,
    /// The chunk's text.
    pub text: String,
    /// An object with `source`, `format`, `chunk` (the index), `start` and
    /// `end` (byte offsets into the text), and for Markdown `heading` (the
    /// nearest heading above the chunk, when there is one).
    pub metadata: Value,
}

impl LoadedText {
    /// A document from text already in memory.
    pub fn new(source: impl Into<String>, format: TextFormat, text: impl Into<String>) -> Self {
        LoadedText {
            source: source.into(),
            format,
            text: text.into(),
        }
    }

    /// Cuts the text with `chunker`. Empty and whitespace-only chunks are
    /// dropped (indices stay those of the chunker).
    ///
    /// ```
    /// use ferroly::vectorstore::{Boundary, Chunker, LoadedText, TextFormat};
    ///
    /// let doc = LoadedText::new(
    ///     "guide.md",
    ///     TextFormat::Markdown,
    ///     "# Setup\n\nInstall it.\n\n# Usage\n\nRun it.\n",
    /// );
    /// let chunks = doc.chunks(&Chunker::new(20).boundary(Boundary::Paragraph));
    /// assert_eq!(chunks[0].id, "guide.md#0");
    /// let last = chunks.last().unwrap();
    /// assert_eq!(last.metadata.get("heading").and_then(|v| v.as_str()), Some("Usage"));
    /// ```
    pub fn chunks(&self, chunker: &Chunker) -> Vec<TextChunk> {
        let headings = match self.format {
            TextFormat::Markdown => markdown_headings(&self.text),
            TextFormat::Plain => Vec::new(),
        };
        chunker
            .chunk(&self.text)
            .into_iter()
            .filter(|c| !c.text.trim().is_empty())
            .map(|c| {
                let mut metadata = vec![
                    ("source".to_string(), Value::Str(self.source.clone())),
                    (
                        "format".to_string(),
                        Value::Str(self.format.label().to_string()),
                    ),
                    ("chunk".to_string(), Value::Int(c.index as i64)),
                    ("start".to_string(), Value::Int(c.start as i64)),
                    ("end".to_string(), Value::Int(c.end as i64)),
                ];
                // The heading in force where the chunk's content begins.
                let first = c.start + (c.text.len() - c.text.trim_start().len());
                let heading = headings
                    .iter()
                    .take_while(|(offset, _)| *offset <= first)
                    .last()
                    .map(|(_, title)| title.clone());
                if let Some(title) = heading {
                    metadata.push(("heading".to_string(), Value::Str(title)));
                }
                TextChunk {
                    id: format!("{}#{}", self.source, c.index),
                    text: c.text.to_string(),
                    metadata: Value::Object(metadata),
                }
            })
            .collect()
    }
}

/// The ATX headings (`# Title` … `###### Title`) of a Markdown text, as
/// `(byte offset of the line, title)`, skipping fenced code blocks.
fn markdown_headings(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut offset = 0;
    let mut fence: Option<&str> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m));
        match (fence, marker) {
            (None, Some(m)) => fence = Some(m),
            (Some(open), Some(m)) if open == m => fence = None,
            (None, None) => {
                let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
                let rest = &trimmed[hashes..];
                if (1..=6).contains(&hashes) && (rest.starts_with(' ') || rest.trim().is_empty()) {
                    let title = rest.trim().trim_end_matches('#').trim();
                    if !title.is_empty() {
                        out.push((offset, title.to_string()));
                    }
                }
            }
            _ => {}
        }
        offset += line.len();
    }
    out
}

/// Limits for [`load_file_with`] and [`load_dir_with`]. [`load_file`] and
/// [`load_dir`] use the defaults.
///
/// ```
/// use ferroly::vectorstore::LoadOptions;
///
/// let opts = LoadOptions::new().max_file_bytes(1 << 20).max_files(500);
/// assert_eq!(opts.file_bytes_limit(), 1 << 20);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadOptions {
    max_file_bytes: u64,
    max_total_bytes: u64,
    max_files: usize,
    max_depth: usize,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions {
            max_file_bytes: LoadOptions::DEFAULT_MAX_FILE_BYTES,
            max_total_bytes: LoadOptions::DEFAULT_MAX_TOTAL_BYTES,
            max_files: LoadOptions::DEFAULT_MAX_FILES,
            max_depth: LoadOptions::DEFAULT_MAX_DEPTH,
        }
    }
}

impl LoadOptions {
    /// The default largest file: 16 MiB.
    pub const DEFAULT_MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
    /// The default most bytes one directory load reads: 256 MiB.
    pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
    /// The default most files one directory load reads: 10 000.
    pub const DEFAULT_MAX_FILES: usize = 10_000;
    /// The default deepest sub-directory a directory load enters: 32.
    pub const DEFAULT_MAX_DEPTH: usize = 32;

    /// The default limits.
    pub fn new() -> Self {
        LoadOptions::default()
    }

    /// The largest file read; a larger one is an error.
    pub fn max_file_bytes(mut self, bytes: u64) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    /// The most bytes a directory load reads in all; past it, an error.
    pub fn max_total_bytes(mut self, bytes: u64) -> Self {
        self.max_total_bytes = bytes;
        self
    }

    /// The most files a directory load reads; more is an error.
    pub fn max_files(mut self, files: usize) -> Self {
        self.max_files = files;
        self
    }

    /// How deep a directory load descends (the directory itself is depth
    /// 0); a deeper sub-directory is an error.
    pub fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    /// The configured largest file.
    pub fn file_bytes_limit(&self) -> u64 {
        self.max_file_bytes
    }

    /// The configured most bytes per directory load.
    pub fn total_bytes_limit(&self) -> u64 {
        self.max_total_bytes
    }

    /// The configured most files per directory load.
    pub fn files_limit(&self) -> usize {
        self.max_files
    }

    /// The configured deepest sub-directory.
    pub fn depth_limit(&self) -> usize {
        self.max_depth
    }
}

/// Reads one UTF-8 text file with the default [`LoadOptions`]. The format
/// comes from the extension (Markdown for `.md` / `.markdown`, plain text
/// otherwise); `source` is the path as given. A symbolic link given as the
/// path is followed, but the target must be a regular file (not a FIFO or
/// device) no larger than the limit.
pub fn load_file(path: impl AsRef<Path>) -> Result<LoadedText, VectorStoreError> {
    load_file_with(path, &LoadOptions::default())
}

/// [`load_file`] with explicit limits.
pub fn load_file_with(
    path: impl AsRef<Path>,
    opts: &LoadOptions,
) -> Result<LoadedText, VectorStoreError> {
    let path = path.as_ref();
    let expected = std::fs::metadata(path)?;
    let bytes = read_regular(path, &expected, opts.max_file_bytes, || {
        file_too_large(path, opts.max_file_bytes)
    })?;
    to_loaded(path, bytes)
}

fn file_too_large(path: &Path, max: u64) -> String {
    format!("{} is larger than the {max}-byte limit", path.display())
}

fn to_loaded(path: &Path, bytes: Vec<u8>) -> Result<LoadedText, VectorStoreError> {
    let text = String::from_utf8(bytes).map_err(|_| {
        VectorStoreError::InvalidInput(format!("{} is not valid UTF-8", path.display()))
    })?;
    Ok(LoadedText {
        source: path.display().to_string(),
        format: TextFormat::for_path(path).unwrap_or(TextFormat::Plain),
        text,
    })
}

/// Reads `path`, which `expected` (taken before opening) says is a regular
/// file, refusing it when the opened file is not that same regular file (a
/// FIFO, a device, or an entry swapped after it was checked) or holds more
/// than `max` bytes (the error then says `too_large()`).
fn read_regular(
    path: &Path,
    expected: &std::fs::Metadata,
    max: u64,
    too_large: impl FnOnce() -> String,
) -> Result<Vec<u8>, VectorStoreError> {
    use std::io::Read;
    let not_regular =
        || VectorStoreError::InvalidInput(format!("{} is not a regular file", path.display()));
    // Checking before opening keeps a FIFO from blocking the open.
    if !expected.is_file() {
        return Err(not_regular());
    }
    let file = std::fs::File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || !same_file(expected, &opened) {
        return Err(not_regular());
    }
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(VectorStoreError::InvalidInput(too_large()));
    }
    Ok(bytes)
}

/// Whether two metadata describe the same file: device and inode on Unix.
/// Elsewhere the identity is not available on the supported toolchains, so
/// only the kind is compared (done by the caller).
#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_a: &std::fs::Metadata, _b: &std::fs::Metadata) -> bool {
    true
}

/// Reads every `.txt`, `.text`, `.md` and `.markdown` file under `dir`,
/// recursively, in path order, with the default [`LoadOptions`]. Hidden
/// entries (names starting with `.`) and symbolic links are skipped, so a
/// link cycle cannot loop, and an entry replaced by a link between the
/// listing and the read is refused (on Unix). A file that is not valid UTF-8,
/// or a limit exceeded, is an error.
pub fn load_dir(dir: impl AsRef<Path>) -> Result<Vec<LoadedText>, VectorStoreError> {
    load_dir_with(dir, &LoadOptions::default())
}

/// [`load_dir`] with explicit limits.
pub fn load_dir_with(
    dir: impl AsRef<Path>,
    opts: &LoadOptions,
) -> Result<Vec<LoadedText>, VectorStoreError> {
    let mut files = Vec::new();
    collect_files(dir.as_ref(), 0, opts, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut total: u64 = 0;
    let mut out = Vec::with_capacity(files.len());
    for (path, expected) in &files {
        let remaining = opts.max_total_bytes.saturating_sub(total);
        let bytes = if remaining < opts.max_file_bytes {
            read_regular(path, expected, remaining, || {
                format!(
                    "reading {} would exceed the {}-byte limit for the whole directory",
                    path.display(),
                    opts.max_total_bytes
                )
            })?
        } else {
            read_regular(path, expected, opts.max_file_bytes, || {
                file_too_large(path, opts.max_file_bytes)
            })?
        };
        total += bytes.len() as u64;
        out.push(to_loaded(path, bytes)?);
    }
    Ok(out)
}

fn collect_files(
    dir: &Path,
    depth: usize,
    opts: &LoadOptions,
    out: &mut Vec<(PathBuf, std::fs::Metadata)>,
) -> Result<(), VectorStoreError> {
    if depth > opts.max_depth {
        return Err(VectorStoreError::InvalidInput(format!(
            "{} is deeper than the limit of {} directories",
            dir.display(),
            opts.max_depth
        )));
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            collect_files(&path, depth + 1, opts, out)?;
        } else if kind.is_file() && TextFormat::for_path(&path).is_some() {
            if out.len() >= opts.max_files {
                return Err(VectorStoreError::InvalidInput(format!(
                    "the directory holds more than {} loadable files",
                    opts.max_files
                )));
            }
            // Not following links: the entry as listed, to compare with the
            // file that is opened later.
            let meta = std::fs::symlink_metadata(&path)?;
            out.push((path, meta));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vectorstore::Boundary;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ferroly-loader-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn headings_skip_code_fences() {
        let text = "# One\n\n```\n# not a heading\n```\n\n## Two ##\n#tag\n";
        let headings = markdown_headings(text);
        let titles: Vec<&str> = headings.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(titles, ["One", "Two"]);
    }

    #[test]
    fn chunks_carry_source_offsets_and_heading() {
        let text = "# Intro\n\nFirst paragraph.\n\n# Details\n\nSecond paragraph.\n";
        let doc = LoadedText::new("doc.md", TextFormat::Markdown, text);
        let chunks = doc.chunks(&Chunker::new(30).boundary(Boundary::Paragraph));
        assert!(chunks.len() >= 2);
        for c in &chunks {
            let start = c.metadata.get("start").unwrap().as_i64().unwrap() as usize;
            let end = c.metadata.get("end").unwrap().as_i64().unwrap() as usize;
            assert_eq!(&text[start..end], c.text);
            assert_eq!(c.metadata.get("source").unwrap().as_str(), Some("doc.md"));
        }
        let second = chunks.iter().find(|c| c.text.contains("Second")).unwrap();
        assert_eq!(
            second.metadata.get("heading").unwrap().as_str(),
            Some("Details")
        );
        let plain = LoadedText::new("a.txt", TextFormat::Plain, "# not markdown");
        let chunks = plain.chunks(&Chunker::new(100));
        assert!(chunks[0].metadata.get("heading").is_none());
    }

    #[test]
    fn loads_a_directory_in_path_order() {
        let dir = scratch("dir");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join("b.md"), "# B\n\nbee\n").unwrap();
        std::fs::write(dir.join("a.txt"), "ay").unwrap();
        std::fs::write(dir.join("sub").join("c.markdown"), "see").unwrap();
        std::fs::write(dir.join("skip.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join(".hidden").join("d.md"), "hidden").unwrap();
        let docs = load_dir(&dir).unwrap();
        let names: Vec<String> = docs
            .iter()
            .map(|d| {
                Path::new(&d.source)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["a.txt", "b.md", "c.markdown"]);
        assert_eq!(docs[0].format, TextFormat::Plain);
        assert_eq!(docs[1].format, TextFormat::Markdown);

        std::fs::write(dir.join("bad.txt"), [0xff, 0xfe]).unwrap();
        assert!(matches!(
            load_dir(&dir),
            Err(VectorStoreError::InvalidInput(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn invalid(result: Result<impl std::fmt::Debug, VectorStoreError>) -> String {
        match result {
            Err(VectorStoreError::InvalidInput(m)) => m,
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn limits_bound_file_size_total_bytes_count_and_depth() {
        let dir = scratch("limits");
        std::fs::write(dir.join("a.txt"), "x".repeat(100)).unwrap();
        std::fs::write(dir.join("b.txt"), "y".repeat(100)).unwrap();

        // One file over the per-file limit.
        let small = LoadOptions::new().max_file_bytes(99);
        assert!(invalid(load_file_with(dir.join("a.txt"), &small)).contains("99-byte limit"));
        assert!(invalid(load_dir_with(&dir, &small)).contains("99-byte limit"));
        // Exactly at the limit is fine.
        let exact = LoadOptions::new().max_file_bytes(100);
        assert_eq!(
            load_file_with(dir.join("a.txt"), &exact)
                .unwrap()
                .text
                .len(),
            100
        );

        // The total across files.
        let total = LoadOptions::new().max_total_bytes(150);
        assert!(invalid(load_dir_with(&dir, &total)).contains("whole directory"));
        assert_eq!(
            load_dir_with(&dir, &LoadOptions::new().max_total_bytes(200))
                .unwrap()
                .len(),
            2
        );

        // The number of files.
        assert!(
            invalid(load_dir_with(&dir, &LoadOptions::new().max_files(1))).contains("more than 1")
        );

        // The depth.
        std::fs::create_dir_all(dir.join("x").join("y")).unwrap();
        std::fs::write(dir.join("x").join("y").join("c.md"), "deep").unwrap();
        assert!(invalid(load_dir_with(&dir, &LoadOptions::new().max_depth(1))).contains("deeper"));
        assert_eq!(
            load_dir_with(&dir, &LoadOptions::new().max_depth(2))
                .unwrap()
                .len(),
            3
        );

        // A directory is not a regular file.
        assert!(invalid(load_file(dir.join("x"))).contains("not a regular file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn links_inside_a_directory_are_not_read() {
        let dir = scratch("links");
        let outside = scratch("links-outside");
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::fs::write(dir.join("ok.txt"), "ok").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("link.txt")).unwrap();
        let docs = load_dir(&dir).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].text, "ok");

        // An entry listed as a regular file and then replaced by a link is
        // refused when it is read.
        let listed = std::fs::symlink_metadata(dir.join("ok.txt")).unwrap();
        std::fs::remove_file(dir.join("ok.txt")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("ok.txt")).unwrap();
        let swapped = read_regular(&dir.join("ok.txt"), &listed, 1024, String::new);
        assert!(invalid(swapped).contains("not a regular file"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
