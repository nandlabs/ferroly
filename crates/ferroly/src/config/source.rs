//! Configuration sources: files, environment variables and raw arguments.

use std::path::{Path, PathBuf};

use ferroly::codec::{json, toml, xml, yaml, Value};

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

/// Largest secret file read for the `*_FILE` convention.
const MAX_SECRET_BYTES: u64 = 1024 * 1024;

/// What replaces file content that an error message would otherwise repeat.
const REDACTED: &str = "<redacted>";

#[derive(Clone, Copy)]
enum FileFormat {
    Json,
    Yaml,
    Toml,
    Xml,
    Properties,
}

fn format_of(path: &Path) -> Option<FileFormat> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => Some(FileFormat::Json),
        Some("yaml") | Some("yml") => Some(FileFormat::Yaml),
        Some("toml") => Some(FileFormat::Toml),
        Some("xml") => Some(FileFormat::Xml),
        Some("properties") | Some("props") | Some("env") => Some(FileFormat::Properties),
        _ => None,
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
/// failure (unknown extension, unreadable file, malformed content) is an
/// error either way.
pub(super) fn load_file(path: &Path, optional: bool) -> Result<Option<Value>, ConfigError> {
    // Validate the extension before touching the filesystem.
    let format = format_of(path)
        .ok_or_else(|| ConfigError::UnsupportedFormat(path.display().to_string()))?;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if optional && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(path, e)),
    };
    let parsed = match format {
        FileFormat::Json => json::from_slice(&bytes),
        FileFormat::Yaml => yaml::from_str(text(path, &bytes)?),
        FileFormat::Toml => toml::from_str(text(path, &bytes)?),
        FileFormat::Xml => xml::from_str(text(path, &bytes)?),
        FileFormat::Properties => Ok(parse_properties(text(path, &bytes)?)),
    };
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
    let mut entries: Vec<(String, Value)> = Vec::new();
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
            if !segs.iter().any(|s| s.is_empty()) {
                insert_nested(&mut entries, &segs, Value::Str(val.trim().to_string()));
            }
        }
    }
    Value::Object(entries)
}

/// One setting produced by a source, with where it came from.
pub(super) struct Setting {
    pub(super) segs: Vec<String>,
    pub(super) value: Value,
    pub(super) source: Source,
}

/// Reads the environment variables starting with `prefix` into settings.
///
/// `base` is the configuration merged so far; it is only consulted for
/// [`EnvOptions::case_insensitive`] key matching.
pub(super) fn read_env(
    prefix: &str,
    opts: &EnvOptions,
    base: &Value,
) -> Result<Vec<Setting>, ConfigError> {
    // Variables whose name or value is not valid UTF-8 cannot be config
    // values; skip them instead of panicking the way `env::vars()` would.
    //
    // Process in sorted key order so the result is deterministic regardless
    // of the environment's arbitrary iteration order. On a scalar-vs-nested
    // collision (`APP_DB` and `APP_DB__HOST`) the nested form wins, since its
    // longer key sorts last and is applied last; for the same reason
    // `APP_X_FILE` wins over `APP_X`.
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    vars.sort();

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
        // An empty segment (`APP_A____B`, a leading or trailing `__`) has no
        // sensible key; ignore the variable rather than invent a "" key.
        if key.is_empty() || key.split("__").any(str::is_empty) {
            continue;
        }
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
        if opts.match_keys {
            remap(base, &mut segs);
        }

        let (text, source) = if secret_key.is_some() {
            (
                read_secret(&name, Path::new(&raw))?,
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
    let fail = |e: &dyn std::fmt::Display| {
        ConfigError::Io(format!("secret file for {var}: {}: {e}", path.display()))
    };
    let meta = std::fs::metadata(path).map_err(|e| fail(&e))?;
    if meta.len() > MAX_SECRET_BYTES {
        return Err(fail(&"file is larger than 1 MiB"));
    }
    let bytes = std::fs::read(path).map_err(|e| fail(&e))?;
    let text = String::from_utf8(bytes).map_err(|_| fail(&"content is not valid UTF-8"))?;
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
        if !segs.iter().any(|s| s.is_empty()) {
            out.push((segs, Value::Str(val)));
        }
    }
    out
}

/// Sets `segs` (a non-empty key path) to `value` inside `entries`, creating
/// intermediate objects and replacing any non-object in the way.
pub(super) fn insert_nested(entries: &mut Vec<(String, Value)>, segs: &[String], value: Value) {
    let head = &segs[0];
    if segs.len() == 1 {
        match entries.iter_mut().find(|(k, _)| k == head) {
            Some((_, v)) => *v = value,
            None => entries.push((head.clone(), value)),
        }
        return;
    }
    let idx = match entries.iter().position(|(k, _)| k == head) {
        Some(i) => i,
        None => {
            entries.push((head.clone(), Value::Object(Vec::new())));
            entries.len() - 1
        }
    };
    if !matches!(entries[idx].1, Value::Object(_)) {
        entries[idx].1 = Value::Object(Vec::new());
    }
    if let Value::Object(child) = &mut entries[idx].1 {
        insert_nested(child, &segs[1..], value);
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
}
