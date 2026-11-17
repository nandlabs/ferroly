//! Configuration sources: files, environment variables and raw arguments.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ferroly::codec::{json, toml, xml, yaml, Limits, Value};

use super::secret::{read_bounded, read_secret_file};
use super::unknown::EnvKey;
use super::{navigate, ConfigError, EnvOptions, Source};

/// The file extensions [`load_file`] understands, in probing order.
pub(super) const EXTENSIONS: [&str; 8] = [
    "json",
    "yaml",
    "yml",
    "toml",
    "xml",
    "properties",
    "props",
    "env",
];

/// What replaces file content that an error message would otherwise repeat.
const REDACTED: &str = "<redacted>";

/// The most segments a key read from arguments, `cli` flags, the environment,
/// a secrets directory or a `.properties` file may have: the nesting depth the
/// codec allows a config file ([`Limits::DEFAULT_MAX_DEPTH`]). A deeper key is
/// ignored, like a key with an empty segment — building, merging and dropping
/// a tree that deep would overflow the stack.
pub(super) const MAX_KEY_DEPTH: usize = Limits::DEFAULT_MAX_DEPTH;

/// Whether `segs` can be a config key: 1 to [`MAX_KEY_DEPTH`] segments, none
/// of them empty.
pub(super) fn usable_key(segs: &[String]) -> bool {
    !segs.is_empty() && segs.len() <= MAX_KEY_DEPTH && !segs.iter().any(String::is_empty)
}

/// A config file format, chosen from the file extension.
///
/// Give a list of them to [`ConfigBuilder::formats`](super::ConfigBuilder::formats)
/// to restrict which files a builder loads.
///
/// ```
/// use std::path::Path;
/// use ferroly::config::FileFormat;
///
/// assert_eq!(FileFormat::from_path(Path::new("app.YML")), Some(FileFormat::Yaml));
/// assert_eq!(FileFormat::Properties.extensions(), ["properties", "props", "env"]);
/// assert_eq!(FileFormat::from_path(Path::new("app.ini")), None);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FileFormat {
    /// JSON: `.json`.
    Json,
    /// YAML: `.yaml`, `.yml`.
    Yaml,
    /// TOML: `.toml`.
    Toml,
    /// XML: `.xml`.
    Xml,
    /// Java-style properties (`key = value`, dots nest): `.properties`,
    /// `.props`, `.env`.
    Properties,
}

impl FileFormat {
    /// Every format, in probing order (the order in which
    /// [`merge_profile`](super::ConfigBuilder::merge_profile) tries their
    /// extensions).
    pub fn all() -> &'static [FileFormat] {
        &[
            FileFormat::Json,
            FileFormat::Yaml,
            FileFormat::Toml,
            FileFormat::Xml,
            FileFormat::Properties,
        ]
    }

    /// The format the extension of `path` maps to, ignoring ASCII case
    /// (`app.YML` is YAML); `None` for a missing or unknown extension.
    pub fn from_path(path: &Path) -> Option<FileFormat> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        FileFormat::all()
            .iter()
            .copied()
            .find(|f| f.extensions().contains(&ext.as_str()))
    }

    /// The extensions of this format, without the dot, in probing order:
    /// `Json` `["json"]`, `Yaml` `["yaml", "yml"]`, `Toml` `["toml"]`, `Xml`
    /// `["xml"]`, `Properties` `["properties", "props", "env"]`.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            FileFormat::Json => &["json"],
            FileFormat::Yaml => &["yaml", "yml"],
            FileFormat::Toml => &["toml"],
            FileFormat::Xml => &["xml"],
            FileFormat::Properties => &["properties", "props", "env"],
        }
    }
}

fn format_of(path: &Path) -> Option<FileFormat> {
    FileFormat::from_path(path)
}

/// The extensions of the `allowed` formats (every format for `None`),
/// without the dot, in [`EXTENSIONS`] order.
pub(super) fn allowed_extensions(allowed: Option<&[FileFormat]>) -> Vec<&'static str> {
    EXTENSIONS
        .iter()
        .copied()
        .filter(|ext| {
            allowed.map_or(true, |list| {
                list.iter().any(|f| f.extensions().contains(ext))
            })
        })
        .collect()
}

/// [`ConfigError::FormatNotAllowed`] for `path`, listing the extensions of
/// `allowed`.
pub(super) fn not_allowed(path: &Path, allowed: &[FileFormat]) -> ConfigError {
    ConfigError::FormatNotAllowed {
        path: path.display().to_string(),
        allowed: allowed_extensions(Some(allowed))
            .iter()
            .map(|e| format!(".{e}"))
            .collect(),
    }
}

/// Fails with [`ConfigError::FormatNotAllowed`] when `path` has a known
/// extension whose format is not in `allowed` (`None` allows every format).
/// An unknown extension passes: loading reports it as
/// [`ConfigError::UnsupportedFormat`].
pub(super) fn check_allowed(
    path: &Path,
    allowed: Option<&[FileFormat]>,
) -> Result<(), ConfigError> {
    match (format_of(path), allowed) {
        (Some(format), Some(list)) if !list.contains(&format) => Err(not_allowed(path, list)),
        _ => Ok(()),
    }
}

/// Whether `path` ends in one of the supported config extensions.
pub(super) fn has_known_extension(path: &Path) -> bool {
    format_of(path).is_some()
}

/// `path` with `.ext` appended (not replaced).
pub(super) fn with_extension(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Loads and parses a config file, format inferred from the extension.
///
/// With `optional`, a file that does not exist yields `Ok(None)`; every other
/// failure (unknown extension, a format not in `allowed`, unreadable file,
/// malformed content) is an error either way. `allowed: None` accepts every
/// format.
pub(super) fn load_file(
    path: &Path,
    optional: bool,
    allowed: Option<&[FileFormat]>,
) -> Result<Option<Value>, ConfigError> {
    // Validate the extension before touching the filesystem.
    let format = format_of(path)
        .ok_or_else(|| ConfigError::UnsupportedFormat(path.display().to_string()))?;
    if let Some(list) = allowed.filter(|list| !list.contains(&format)) {
        return Err(not_allowed(path, list));
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if optional && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(path, e)),
    };
    // An empty or whitespace-only file (after an optional UTF-8 BOM, which
    // every parser accepts) is "no settings" in every format; the JSON and
    // XML parsers would otherwise reject it as truncated.
    let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Some(Value::Object(Vec::new())));
    }
    let parsed = match format {
        FileFormat::Json => json::from_slice(&bytes),
        FileFormat::Yaml => yaml::from_str(text(path, &bytes)?),
        FileFormat::Toml => toml::from_str(text(path, &bytes)?),
        FileFormat::Xml => xml::from_str(text(path, &bytes)?),
        FileFormat::Properties => Ok(parse_properties(text(path, &bytes)?)),
    };
    // A comment-only document (or a JSON `null`) is "no settings" too, not a
    // `null` that would replace the whole configuration.
    let parsed = parsed.map(|v| match v {
        Value::Null => Value::Object(Vec::new()),
        v => v,
    });
    parsed.map(Some).map_err(|e| ConfigError::Parse {
        path: path.display().to_string(),
        message: scrub(&e.to_string(), &String::from_utf8_lossy(&bytes)),
    })
}

fn io<E: std::fmt::Display>(path: &Path, e: E) -> ConfigError {
    ConfigError::Io(format!("{}: {e}", path.display()))
}

fn text<'a>(path: &Path, bytes: &'a [u8]) -> Result<&'a str, ConfigError> {
    std::str::from_utf8(bytes).map_err(|e| io(path, e))
}

/// Removes anything a parser copied out of the file from its error message:
/// every occurrence in `message` of a line (or line value) of `content` is
/// replaced with a redaction marker.
///
/// A config file routinely holds credentials, and a parse error is exactly the
/// kind of message that ends up in a log, so the offending line must not ride
/// along. The structural part of the message survives, and the line number is
/// added when the echoed text pins it down.
pub(super) fn scrub(message: &str, content: &str) -> String {
    // Fragments shorter than this are too likely to be ordinary words of the
    // message itself.
    const MIN: usize = 3;

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut line_hit: Option<usize> = None;
    let mut mark = |fragment: &str, line: Option<usize>, ranges: &mut Vec<(usize, usize)>| {
        if fragment.len() < MIN {
            return;
        }
        let mut found = false;
        for (start, _) in message.match_indices(fragment) {
            ranges.push((start, start + fragment.len()));
            found = true;
        }
        if found && line_hit.is_none() {
            line_hit = line;
        }
    };

    for (idx, raw) in content.lines().enumerate() {
        let mut line = raw.trim();
        if line.is_empty() {
            continue;
        }
        mark(line, Some(idx + 1), &mut ranges);
        // A YAML sequence item is reported without its leading dash.
        while let Some(rest) = line.strip_prefix("- ") {
            line = rest.trim_start();
            mark(line, Some(idx + 1), &mut ranges);
        }
        // The value on its own, in case only that part is quoted back.
        if let Some(pos) = line.find([':', '=']) {
            let value = line[pos + 1..]
                .trim()
                .trim_matches(|c| c == '"' || c == '\'');
            mark(value, None, &mut ranges);
        }
    }

    if ranges.is_empty() {
        return message.to_string();
    }
    ranges.sort_unstable();
    let mut out = String::with_capacity(message.len());
    let mut pos = 0;
    for (start, end) in ranges {
        if end <= pos {
            continue;
        }
        if start > pos {
            out.push_str(&message[pos..start]);
            out.push_str(REDACTED);
        } else if pos == 0 {
            out.push_str(REDACTED);
        }
        // An overlapping range extends the redaction already written.
        pos = end;
    }
    out.push_str(&message[pos..]);
    if let Some(line) = line_hit {
        if !out.contains("line ") {
            out.push_str(&format!(" (line {line})"));
        }
    }
    out
}

/// Parses `.properties` lines (`key.path = value`, `#`/`!` comments), nesting on
/// `.` — so `db.host = localhost` becomes `{ db: { host: "localhost" } }`.
pub(super) fn parse_properties(input: &str) -> Value {
    let mut tree = Tree::default();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let segs: Vec<String> = key
                .trim()
                .split('.')
                .map(|s| s.trim().to_string())
                .collect();
            if usable_key(&segs) {
                tree.insert(&segs, Value::Str(val.trim().to_string()));
            }
        }
    }
    tree.into_value()
}

/// One setting produced by a source, with where it came from.
pub(super) struct Setting {
    pub(super) segs: Vec<String>,
    pub(super) value: Value,
    pub(super) source: Source,
}

/// Turns environment variables (the process environment's `vars_os()` or an
/// injected list) into the `(name, value)` pairs [`read_env`] reads: sorted
/// by name, one entry per name, all valid UTF-8.
///
/// - Sorted by name, so the result does not depend on the input order. On a
///   scalar-vs-nested collision (`APP_DB` and `APP_DB__HOST`) the nested form
///   wins, since its longer name sorts last and is applied last; for the same
///   reason `APP_X_FILE` wins over `APP_X`.
/// - A name given more than once keeps its last value in input order, as if
///   the variables had been set one after another.
/// - A variable whose name or value is not valid UTF-8 cannot be a config
///   value; it is skipped instead of panicking the way `env::vars()` would.
pub(super) fn collect_vars<I, K, V>(vars: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    // A non-UTF-8 name can never equal a UTF-8 one, so it is dropped first;
    // the value is checked after de-duplication, so a last occurrence with a
    // non-UTF-8 value removes the variable, as setting it would.
    let mut named: Vec<(String, OsString)> = vars
        .into_iter()
        .filter_map(|(k, v)| Some((k.into().into_string().ok()?, v.into())))
        .collect();
    // Stable: equal names stay in input order, so the last one is the latest.
    named.sort_by(|a, b| a.0.cmp(&b.0));
    let mut unique: Vec<(String, OsString)> = Vec::with_capacity(named.len());
    for (name, value) in named {
        match unique.last_mut() {
            Some(last) if last.0 == name => last.1 = value,
            _ => unique.push((name, value)),
        }
    }
    unique
        .into_iter()
        .filter_map(|(k, v)| Some((k, v.into_string().ok()?)))
        .collect()
}

/// Reads the variables of `vars` (from [`collect_vars`]) starting with
/// `prefix` into settings.
///
/// `base` is the configuration merged so far; it is only consulted for
/// [`EnvOptions::case_insensitive`] key matching. `classify` decides, before
/// any secret file is read, whether a key is configuration at all
/// ([`EnvKey::Ignore`] skips the variable) and whether it is known (the file
/// of an [`EnvKey::Unknown`] `*_FILE` variable is not read; its value is
/// `null`).
pub(super) fn read_env(
    prefix: &str,
    vars: Vec<(String, String)>,
    opts: &EnvOptions,
    base: &Value,
    classify: &dyn Fn(&[String]) -> EnvKey,
) -> Result<Vec<Setting>, ConfigError> {
    let mut out = Vec::new();
    for (name, raw) in vars {
        let Some(rest) = name.strip_prefix(prefix) else {
            continue;
        };
        let secret_key = if opts.secret_files {
            rest.strip_suffix("_FILE").filter(|k| !k.is_empty())
        } else {
            None
        };
        let key = secret_key.unwrap_or(rest);
        let mut segs: Vec<String> = key
            .split("__")
            .map(|s| {
                if opts.preserve_case {
                    s.to_string()
                } else {
                    s.to_lowercase()
                }
            })
            .collect();
        // An empty segment (`APP_A____B`, a leading or trailing `__`) has no
        // sensible key, and a key deeper than `MAX_KEY_DEPTH` cannot be
        // merged safely; ignore the variable rather than invent a "" key.
        if !usable_key(&segs) {
            continue;
        }
        if opts.match_keys {
            remap(base, &mut segs);
        }
        let class = classify(&segs);
        if class == EnvKey::Ignore {
            continue;
        }

        let (text, source) = if secret_key.is_some() {
            let text = if class == EnvKey::Unknown {
                String::new()
            } else {
                read_secret(&name, Path::new(&raw))?
            };
            (
                text,
                Source::SecretFile {
                    var: name.clone(),
                    path: raw.clone(),
                },
            )
        } else {
            (raw, Source::Env(name.clone()))
        };

        let dotted = segs.join(".");
        let listed = opts
            .list
            .as_ref()
            .filter(|(_, keys)| keys.iter().any(|k| normalize(k) == normalize(&dotted)))
            .map(|(sep, _)| *sep);
        let split_on = listed.or(opts.split_all.filter(|sep| text.contains(*sep)));
        let value = match split_on {
            Some(sep) => Value::Array(
                text.split(sep)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| Value::Str(s.to_string()))
                    .collect(),
            ),
            None => Value::Str(text),
        };
        out.push(Setting {
            segs,
            value,
            source,
        });
    }
    Ok(out)
}

/// Reads the file a `*_FILE` variable points at, without its trailing newline.
fn read_secret(var: &str, path: &Path) -> Result<String, ConfigError> {
    let text = read_bounded(path)
        .map_err(|e| ConfigError::Io(format!("secret file for {var}: {}: {e}", path.display())))?;
    Ok(text.trim_end_matches(['\n', '\r']).to_string())
}

/// Case- and separator-insensitive form of a key, for matching `MAX_CONNECTIONS`
/// against `maxConnections` or `max-connections`.
fn normalize(key: &str) -> String {
    key.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Rewrites each segment to the spelling of the matching key already present
/// in `base`, walking down as long as segments keep matching.
fn remap(base: &Value, segs: &mut [String]) {
    let mut cur = base;
    for seg in segs.iter_mut() {
        let Value::Object(entries) = cur else {
            return;
        };
        let found = entries
            .iter()
            .find(|(k, _)| k == seg)
            .or_else(|| {
                entries
                    .iter()
                    .find(|(k, _)| k.to_lowercase() == seg.to_lowercase())
            })
            .or_else(|| entries.iter().find(|(k, _)| normalize(k) == normalize(seg)));
        match found {
            Some((k, v)) => {
                seg.clone_from(k);
                cur = v;
            }
            None => return,
        }
    }
}

/// Parses `--key=value` / `--key value` / `--flag` arguments into settings.
///
/// `base` is the configuration merged so far: it decides whether a `--key`
/// without `=` takes the following token as its value (see
/// [`ConfigBuilder::merge_args`](super::ConfigBuilder::merge_args)).
pub(super) fn parse_args<I>(args: I, base: &Value) -> Vec<(Vec<String>, Value)>
where
    I: IntoIterator<Item = String>,
{
    let mut out = Vec::new();
    let mut it = args.into_iter().peekable();
    while let Some(arg) = it.next() {
        if arg == "--" {
            // Everything after the terminator is positional.
            break;
        }
        let Some(flag) = arg.strip_prefix("--") else {
            continue;
        };
        let (key, val) = match flag.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => {
                // Only a key known to hold a non-boolean value consumes the
                // next token; anything else is a switch, so a positional
                // following it is left alone.
                let wants_value = navigate(base, flag).is_some_and(|v| match v {
                    Value::Bool(_) | Value::Null | Value::Object(_) => false,
                    Value::Str(s) => {
                        !matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "false")
                    }
                    _ => true,
                });
                let v = match it.peek() {
                    Some(next) if wants_value && !next.starts_with("--") => {
                        it.next().unwrap_or_default()
                    }
                    _ => "true".to_string(),
                };
                (flag.to_string(), v)
            }
        };
        let segs: Vec<String> = key.split('.').map(str::to_string).collect();
        if usable_key(&segs) {
            out.push((segs, Value::Str(val)));
        }
    }
    out
}

/// Reads a mounted secrets directory: one regular file per key, as Kubernetes
/// and Docker secret volumes lay them out. `Ok(None)` when the directory is
/// missing and `optional`.
///
/// - Entries whose name starts with `.` are skipped (Kubernetes keeps its
///   `..data` and timestamped directories there), and so is anything that is
///   not a regular file after following symlinks (the visible keys of a
///   Kubernetes volume are symlinks into `..data`).
/// - When `dir/..data` exists (a Kubernetes volume), it is resolved **once**
///   and every key is listed and read from that one revision directory, so a
///   load never mixes keys of two revisions during an atomic update. If the
///   revision is swapped and removed while it is being read, the load starts
///   over on the new one (a few times at most).
/// - The file name is the key: `.` and `__` both nest (`db.password` and
///   `db__password` set `db.password`), the case is kept, and a name with an
///   empty segment or more than [`MAX_KEY_DEPTH`] segments is skipped. A key that is also the prefix of another key
///   (`db` next to `db.password`) is an error: one of them would be lost.
/// - The value is the content with **one** trailing newline (`\n` or `\r\n`)
///   removed; it must be UTF-8 and at most 1 MiB.
pub(super) fn read_secret_dir(
    dir: &Path,
    optional: bool,
) -> Result<Option<Vec<Setting>>, ConfigError> {
    // A revision directory can vanish between resolving `..data` and reading
    // it; the next attempt sees the new revision.
    const ATTEMPTS: usize = 3;
    let mut attempt = 1;
    loop {
        let revision = resolve_revision(dir);
        match read_secret_revision(dir, revision.as_deref(), optional) {
            Err(DirError::Vanished(_))
                if attempt < ATTEMPTS && resolve_revision(dir) != revision =>
            {
                attempt += 1;
            }
            Err(DirError::Vanished(e) | DirError::Fatal(e)) => return Err(e),
            Ok(settings) => return Ok(settings),
        }
    }
}

/// The directory `dir/..data` points at — the current revision of a
/// Kubernetes secret or ConfigMap volume — or `None` when there is none.
fn resolve_revision(dir: &Path) -> Option<PathBuf> {
    let data = dir.join("..data");
    match std::fs::read_link(&data) {
        // Kubernetes writes a relative link (`..2026_11_15_…`); `join` keeps
        // an absolute one as it is.
        Ok(target) => Some(dir.join(target)),
        Err(_) if data.is_dir() => Some(data),
        Err(_) => None,
    }
}

/// Why reading one revision of a secret directory failed.
enum DirError {
    /// A file or the revision directory disappeared while it was read.
    Vanished(ConfigError),
    /// Anything else.
    Fatal(ConfigError),
}

/// Reads the keys of `dir` from `revision` (its resolved `..data`) when there
/// is one, else from `dir` itself.
fn read_secret_revision(
    dir: &Path,
    revision: Option<&Path>,
    optional: bool,
) -> Result<Option<Vec<Setting>>, DirError> {
    let fail = |what: &dyn std::fmt::Display, e: &dyn std::fmt::Display| {
        ConfigError::Io(format!("secret directory {}: {what}: {e}", dir.display()))
    };
    let classify = |kind: Option<std::io::ErrorKind>, e: ConfigError| {
        if revision.is_some() && kind == Some(std::io::ErrorKind::NotFound) {
            DirError::Vanished(e)
        } else {
            DirError::Fatal(e)
        }
    };
    let root = revision.unwrap_or(dir);
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if revision.is_none() && optional && e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None)
        }
        Err(e) => return Err(classify(Some(e.kind()), fail(&"cannot be listed", &e))),
    };
    let mut files: Vec<(String, Vec<String>)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| classify(Some(e.kind()), fail(&"cannot be listed", &e)))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        // `metadata` follows symlinks; a dangling one is skipped like any
        // other non-file.
        if !std::fs::metadata(entry.path()).is_ok_and(|meta| meta.is_file()) {
            continue;
        }
        let segs: Vec<String> = name
            .split('.')
            .flat_map(|part| part.split("__"))
            .map(str::to_string)
            .collect();
        if !usable_key(&segs) {
            continue;
        }
        files.push((name, segs));
    }
    // Deterministic order, so a key spelled two ways resolves the same way
    // every time (the later name wins).
    files.sort();

    // A file whose key is the prefix of another file's key (`db` next to
    // `db.password`) cannot be merged: one value would silently replace the
    // other.
    let keys: HashSet<&[String]> = files.iter().map(|(_, segs)| segs.as_slice()).collect();
    for (name, segs) in &files {
        if let Some(n) = (1..segs.len()).find(|&n| keys.contains(&segs[..n])) {
            let prefix = &segs[..n];
            let owner = files
                .iter()
                .find(|(_, other)| other.as_slice() == prefix)
                .map_or("", |(other, _)| other.as_str());
            return Err(DirError::Fatal(fail(
                &format!("files `{owner}` and `{name}`"),
                &format!(
                    "key `{}` cannot be both a value and a section",
                    prefix.join(".")
                ),
            )));
        }
    }

    let mut out = Vec::with_capacity(files.len());
    for (name, segs) in files {
        let shown = dir.join(&name).display().to_string();
        let text = read_secret_file(&root.join(&name))
            .map_err(|e| classify(e.kind(), fail(&shown, &e)))?;
        out.push(Setting {
            segs,
            value: Value::Str(text),
            source: Source::SecretDir(shown),
        });
    }
    Ok(Some(out))
}

/// An object built from many key paths. Every level indexes its keys, so
/// adding `n` distinct keys takes linear time, and a path is walked with a
/// loop rather than recursion.
#[derive(Default)]
pub(super) struct Tree {
    entries: Vec<(String, Node)>,
    index: HashMap<String, usize>,
}

/// A value in a [`Tree`]: set by a key path, or a section other paths go
/// through.
enum Node {
    Leaf(Value),
    Branch(Tree),
}

impl Tree {
    /// Sets `segs` to `value`, creating the sections on the way and replacing
    /// any value in the way; a later value for the same key replaces the
    /// earlier one, in place. A path that is empty or deeper than
    /// [`MAX_KEY_DEPTH`] is ignored.
    pub(super) fn insert(&mut self, segs: &[String], value: Value) {
        if segs.len() > MAX_KEY_DEPTH {
            return;
        }
        let Some((last, path)) = segs.split_last() else {
            return;
        };
        let mut cur = self;
        for seg in path {
            let node = cur.slot(seg);
            if !matches!(node, Node::Branch(_)) {
                *node = Node::Branch(Tree::default());
            }
            cur = match node {
                Node::Branch(next) => next,
                Node::Leaf(_) => unreachable!("replaced by a branch above"),
            };
        }
        *cur.slot(last) = Node::Leaf(value);
    }

    /// The node for `key` at this level, added (as `null`) when missing.
    fn slot(&mut self, key: &str) -> &mut Node {
        let at = match self.index.get(key) {
            Some(&at) => at,
            None => {
                self.index.insert(key.to_string(), self.entries.len());
                self.entries
                    .push((key.to_string(), Node::Leaf(Value::Null)));
                self.entries.len() - 1
            }
        };
        &mut self.entries[at].1
    }

    /// The tree as a [`Value::Object`], keys in first-insertion order.
    pub(super) fn into_value(self) -> Value {
        Value::Object(
            self.entries
                .into_iter()
                .map(|(key, node)| match node {
                    Node::Leaf(value) => (key, value),
                    Node::Branch(tree) => (key, tree.into_value()),
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_properties_nested() {
        let v = parse_properties("# comment\ndb.host = localhost\ndb.port = 5432\nname=svc\n");
        assert_eq!(v.get("name").and_then(Value::as_str), Some("svc"));
        let db = v.get("db").unwrap();
        assert_eq!(db.get("host").and_then(Value::as_str), Some("localhost"));
        assert_eq!(db.get("port").and_then(Value::as_str), Some("5432"));
    }

    #[test]
    fn scrub_removes_echoed_lines_and_values() {
        let content = "name: svc\npassword s3cr3t-value\n- token: abc123xyz\n";
        let msg = "expected 'key: value', got: password s3cr3t-value";
        let out = scrub(msg, content);
        assert_eq!(out, "expected 'key: value', got: <redacted> (line 2)");

        // A sequence item is echoed without its dash; the value alone too.
        assert_eq!(
            scrub("bad item token: abc123xyz", content),
            "bad item <redacted> (line 3)"
        );
        assert_eq!(
            scrub("bad value abc123xyz here", content),
            "bad value <redacted> here"
        );

        // Nothing echoed: the message is untouched.
        assert_eq!(
            scrub("unexpected end of input", content),
            "unexpected end of input"
        );
    }

    #[test]
    fn scrub_merges_overlapping_fragments() {
        let content = "key: aaa-bbb\nbbb-ccc\n";
        assert_eq!(
            scrub("got aaa-bbb-ccc!", content),
            "got <redacted>! (line 2)"
        );
    }

    #[test]
    fn file_format_tables_agree() {
        let flat: Vec<&str> = FileFormat::all()
            .iter()
            .flat_map(|f| f.extensions().iter().copied())
            .collect();
        assert_eq!(flat, EXTENSIONS);
        assert_eq!(allowed_extensions(None), EXTENSIONS);
    }

    #[test]
    fn file_format_from_path_is_case_insensitive() {
        assert_eq!(
            FileFormat::from_path(Path::new("A.YML")),
            Some(FileFormat::Yaml)
        );
        assert_eq!(
            FileFormat::from_path(Path::new("x.env")),
            Some(FileFormat::Properties)
        );
        assert_eq!(FileFormat::from_path(Path::new("x.ini")), None);
        assert_eq!(FileFormat::from_path(Path::new("noext")), None);
    }

    #[test]
    fn collect_vars_sorts_and_keeps_the_last_duplicate() {
        let vars = collect_vars([("B", "1"), ("A", "2"), ("B", "3"), ("A", "4")]);
        assert_eq!(
            vars,
            [
                ("A".to_string(), "4".to_string()),
                ("B".to_string(), "3".to_string())
            ]
        );
        assert!(collect_vars(Vec::<(String, String)>::new()).is_empty());
    }
}
