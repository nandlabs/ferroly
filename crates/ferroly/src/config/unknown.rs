//! Unknown-key detection: every layer is compared with a schema value as it
//! is merged ([`ConfigBuilder::schema`]).

use std::borrow::Cow;
use std::fmt;
use std::path::Path;

use ferroly::codec::{find, Value};

use super::source::Setting;
use super::{Config, ConfigBuilder, ConfigError, Source};

/// Paths longer than this many characters get no suggestion.
const MAX_SUGGEST_CHARS: usize = 256;
/// At most this many schema paths (in schema order) are offered as
/// suggestions.
const MAX_CANDIDATES: usize = 4096;
/// Edit-distance cells one builder may spend on suggestions in total; keys
/// found after it is spent get none.
const SUGGEST_WORK: usize = 2_000_000;
/// A key longer than this many characters is cut short in messages.
const MAX_SHOWN_CHARS: usize = 200;

/// A configuration key that the [schema](ConfigBuilder::schema) does not
/// know, found in one layer.
///
/// `Display` gives one fragment of the [`ConfigError::UnknownKeys`] message:
///
/// ```text
/// `serve.port` in /etc/app.yaml (did you mean `server.port`?)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnknownKey {
    /// The full dotted key path: `engine.thread`.
    pub path: String,
    /// The key as its source writes it: `engine.thread` in a file or value
    /// layer, `APP_ENGINE__THREAD` in the environment, `--engine.thread` for
    /// a raw argument or a dynamic flag, the argument's name
    /// (`engine.thread`) for an argument declared with `ferroly::cli`, the
    /// file path for a secrets directory.
    pub spelled: String,
    /// The layer the key came from.
    pub source: Source,
    /// The nearest valid section or key, spelled the same way as
    /// [`spelled`](UnknownKey::spelled); `None` when nothing is close, when
    /// the path is longer than 256 characters, or when the builder has
    /// already spent its suggestion budget on earlier keys.
    pub suggestion: Option<String>,
    /// The key is one literal name with dots in it (`"engine.threads": 9`)
    /// that spells a valid nested key: it must be written nested
    /// (`engine: { threads: 9 }`). [`suggestion`](UnknownKey::suggestion) is
    /// then `None`.
    pub dotted: bool,
}

impl fmt::Display for UnknownKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`", shorten(&self.spelled))?;
        match &self.source {
            Source::File(path) => write!(f, " in {path}")?,
            Source::Value => f.write_str(" in a value layer")?,
            _ => {}
        }
        if self.dotted {
            f.write_str(" (one key name with dots in it: write it as nested keys)")?;
        } else if let Some(s) = &self.suggestion {
            write!(f, " (did you mean `{s}`?)")?;
        }
        Ok(())
    }
}

/// `text`, cut to [`MAX_SHOWN_CHARS`] characters with its length appended
/// when it is longer.
fn shorten(text: &str) -> Cow<'_, str> {
    match text.char_indices().nth(MAX_SHOWN_CHARS) {
        None => text.into(),
        Some((end, _)) => format!("{}… ({} chars)", &text[..end], text.chars().count()).into(),
    }
}

/// How the environment layer being screened spells its keys.
pub(super) struct EnvSpelling<'a> {
    /// The prefix given to `merge_env`.
    pub(super) prefix: &'a str,
    /// Whether key segments are written in upper case (no `preserve_case`).
    pub(super) upper: bool,
}

/// Which kind of layer [`Check::screen_settings`] is screening.
pub(super) enum Screen<'a> {
    /// Every setting is configuration: raw arguments, dynamic flags, secret
    /// directories.
    All,
    /// The environment: settings outside the schema's top-level keys are not
    /// configuration.
    Env(EnvSpelling<'a>),
    /// Arguments declared with `ferroly::cli`: like the environment, one
    /// outside the schema's top-level keys (`--config`, `--verbose`) is not
    /// configuration. The others are spelled by their argument name, since
    /// the flag (or variable) that set it may be spelled differently.
    #[cfg(feature = "cli")]
    Declared,
}

impl Screen<'_> {
    fn env(&self) -> Option<&EnvSpelling<'_>> {
        match self {
            Screen::Env(env) => Some(env),
            _ => None,
        }
    }

    /// Whether settings outside the schema's top-level keys are skipped.
    fn skips_outside(&self) -> bool {
        !matches!(self, Screen::All)
    }
}

/// A schema path offered as a suggestion.
struct Candidate {
    segs: Vec<String>,
    chars: Vec<char>,
}

/// The unknown-key state of a [`ConfigBuilder`].
pub(super) struct Check {
    schema: Option<Value>,
    /// The schema's section and key paths, built once by `schema(..)`.
    candidates: Vec<Candidate>,
    /// What is left of [`SUGGEST_WORK`].
    work: usize,
    lenient: bool,
    found: Vec<UnknownKey>,
}

impl Default for Check {
    fn default() -> Self {
        Check {
            schema: None,
            candidates: Vec::new(),
            work: SUGGEST_WORK,
            lenient: false,
            found: Vec::new(),
        }
    }
}

impl Check {
    /// The unknown keys found so far, leaving none behind.
    pub(super) fn take_found(&mut self) -> Vec<UnknownKey> {
        std::mem::take(&mut self.found)
    }

    fn set_schema(&mut self, schema: Value) {
        let mut paths = Vec::new();
        schema_paths(&schema, &mut Vec::new(), &mut paths);
        self.candidates = paths
            .into_iter()
            .map(|segs| Candidate {
                chars: segs.join(".").chars().collect(),
                segs,
            })
            .collect();
        self.schema = Some(schema);
    }

    /// Removes and records the keys of a whole layer that the schema does not
    /// know.
    pub(super) fn screen_value(&mut self, layer: Value, source: &Source) -> Value {
        let Some(schema) = &self.schema else {
            return layer;
        };
        let mut paths = Vec::new();
        let layer = strip(layer, schema, &mut Vec::new(), &mut paths)
            .unwrap_or_else(|| Value::Object(Vec::new()));
        self.record(paths, source, &Screen::All);
        layer
    }

    /// Removes and records the settings the schema does not know. For the
    /// environment and declared arguments, settings whose first segment is
    /// not a top-level key of the schema are not configuration and are
    /// dropped without being recorded.
    pub(super) fn screen_settings(
        &mut self,
        settings: Vec<Setting>,
        screen: &Screen<'_>,
    ) -> Vec<Setting> {
        let Some(schema) = &self.schema else {
            return settings;
        };
        let top = match schema {
            Value::Object(top) if !top.is_empty() => top,
            // A free-form schema accepts everything.
            _ => return settings,
        };
        let mut kept = Vec::with_capacity(settings.len());
        let mut unknown: Vec<(Vec<Vec<String>>, Source)> = Vec::new();
        for mut setting in settings {
            if screen.skips_outside() && find(top, &setting.segs[0]).is_none() {
                continue;
            }
            if let Some(node) = walk(schema, &setting.segs) {
                let mut paths = Vec::new();
                let mut path = setting.segs.clone();
                let value = strip(setting.value, node, &mut path, &mut paths);
                if !paths.is_empty() {
                    unknown.push((paths, setting.source.clone()));
                }
                if let Some(value) = value {
                    setting.value = value;
                    kept.push(setting);
                }
            } else {
                // One setting is one key, whatever its value holds.
                unknown.push((vec![setting.segs], setting.source));
            }
        }
        for (paths, source) in unknown {
            self.record(paths, &source, screen);
        }
        kept
    }

    /// Whether an environment key is configuration, and known: decided before
    /// the variable's value is used, so a `*_FILE` variable that is not
    /// configuration is never read.
    pub(super) fn classify_env(&self, segs: &[String]) -> EnvKey {
        let Some(schema) = &self.schema else {
            return EnvKey::Known;
        };
        match schema {
            Value::Object(top) if !top.is_empty() => {
                if find(top, &segs[0]).is_none() {
                    EnvKey::Ignore
                } else {
                    match walk(schema, segs) {
                        // The path names a section, not a key: someone
                        // else's variable, which must not replace it.
                        Some(Value::Object(entries)) if !entries.is_empty() => EnvKey::Ignore,
                        Some(_) => EnvKey::Known,
                        None => EnvKey::Unknown,
                    }
                }
            }
            _ => EnvKey::Known,
        }
    }

    fn record(&mut self, paths: Vec<Vec<String>>, source: &Source, screen: &Screen<'_>) {
        if paths.is_empty() || self.schema.is_none() {
            return;
        }
        let env = screen.env();
        for segs in paths {
            let path = segs.join(".");
            let spelled = match (source, env) {
                (Source::Env(var), Some(_)) | (Source::SecretFile { var, .. }, Some(_)) => {
                    var.clone()
                }
                (Source::SecretDir(file), _) => file.clone(),
                _ => spell(&segs, source, screen),
            };
            let near = suggest(&path, &self.candidates, &mut self.work);
            let suggestion = near.map(|(c, _)| spell(&c.segs, source, screen));
            // Distance 0: the path spells a schema key, so one segment holds
            // a literal dot. Where the source nests some other way (`__` in
            // the environment) the suggestion shows how; where it would read
            // exactly like the key, say it in words instead.
            let dotted =
                near.is_some_and(|(_, d)| d == 0) && suggestion.as_deref() == Some(&*spelled);
            self.found.push(UnknownKey {
                path,
                spelled,
                source: source.clone(),
                suggestion: if dotted { None } else { suggestion },
                dotted,
            });
        }
    }
}

/// What [`Check::classify_env`] decided about an environment key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EnvKey {
    /// Configuration the schema knows (or no schema): read and merge.
    Known,
    /// Configuration the schema does not know: report, do not read a secret.
    Unknown,
    /// Not configuration (outside the schema's top-level keys, or naming a
    /// section rather than a key): skip.
    Ignore,
}

/// Whether a schema node accepts any keys below it: `null` (an `Option`
/// that defaults to `None`) and an empty object (a free-form map).
fn open(node: &Value) -> bool {
    match node {
        Value::Null => true,
        Value::Object(entries) => entries.is_empty(),
        _ => false,
    }
}

/// The schema node at `segs`, or `None` if the schema does not know the
/// path. An [open](open) node accepts anything below it; a scalar or list
/// leaf accepts nothing below it.
fn walk<'a>(schema: &'a Value, segs: &[String]) -> Option<&'a Value> {
    let mut node = schema;
    for seg in segs {
        match node {
            Value::Object(entries) if !entries.is_empty() => node = find(entries, seg)?,
            n if open(n) => break,
            _ => return None,
        }
    }
    Some(node)
}

/// `value` without the keys `schema` does not know, or `None` when nothing of
/// it is valid; the leaf paths of what was removed are pushed onto `out`.
/// `path` is the position of `value`.
fn strip(
    value: Value,
    schema: &Value,
    path: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) -> Option<Value> {
    match (schema, value) {
        (Value::Object(known), Value::Object(entries)) if !known.is_empty() => {
            let written = !entries.is_empty();
            let reported = out.len();
            let mut kept = Vec::with_capacity(entries.len());
            for (k, v) in entries {
                path.push(k);
                match find(known, path.last().map_or("", String::as_str)) {
                    Some(sub) => {
                        let v = strip(v, sub, path, out);
                        let k = path.pop().unwrap_or_default();
                        if let Some(v) = v {
                            kept.push((k, v));
                        }
                    }
                    None => {
                        leaf_paths(&v, path, out);
                        path.pop();
                    }
                }
            }
            // A section whose every entry was a dropped null section
            // (`server:` holding only an empty `limits:`) is "no settings
            // here" too, at any depth. An explicit `{}` is kept.
            if written && kept.is_empty() && out.len() == reported {
                return None;
            }
            Some(Value::Object(kept))
        }
        // Keys below a scalar or list leaf (`port.x` where the schema has
        // `port: 8080`) are unknown: they would replace the valid key.
        (leaf, Value::Object(entries)) if !entries.is_empty() && !open(leaf) => {
            leaf_paths(&Value::Object(entries), path, out);
            None
        }
        // A section written with nothing under it (`engine:` with every key
        // commented out, an empty file) is "no settings here", not "erase
        // the section": drop it, so the merge leaves the lower layers' values
        // in place. It is not an unknown key either.
        (Value::Object(known), Value::Null) if !known.is_empty() => None,
        // An open node (including `null` for an `Option` that defaults to
        // `None`), a matching leaf, or a non-null scalar where the schema has
        // a section (an enum variant): left for decoding to accept or report.
        (_, value) => Some(value),
    }
}

/// Pushes the path of every leaf of `value` (anything that is not a
/// non-empty object) below `path`.
fn leaf_paths(value: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match value {
        Value::Object(entries) if !entries.is_empty() => {
            for (k, v) in entries {
                path.push(k.clone());
                leaf_paths(v, path, out);
                path.pop();
            }
        }
        _ => out.push(path.clone()),
    }
}

/// Every section and key path of the schema, in schema order, up to
/// [`MAX_CANDIDATES`].
fn schema_paths(schema: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    if let Value::Object(entries) = schema {
        for (k, v) in entries {
            if out.len() >= MAX_CANDIDATES {
                return;
            }
            path.push(k.clone());
            out.push(path.clone());
            schema_paths(v, path, out);
            path.pop();
        }
    }
}

/// The candidate nearest to `path` by Levenshtein distance over whole dotted
/// paths, with that distance, accepted within one edit per four characters
/// of `path` (at least one, at most three). The first of equally near
/// candidates wins.
///
/// The cost is bounded: a path longer than [`MAX_SUGGEST_CHARS`] gets no
/// suggestion, candidates whose length differs by more than the budget are
/// skipped, the distance is computed only within the budget's band, and
/// every cell computed is taken from `work`; once that is spent there are no
/// more suggestions.
fn suggest<'a>(
    path: &str,
    candidates: &'a [Candidate],
    work: &mut usize,
) -> Option<(&'a Candidate, usize)> {
    if path.len() > MAX_SUGGEST_CHARS * 4 {
        return None;
    }
    let path: Vec<char> = path.chars().collect();
    if path.len() > MAX_SUGGEST_CHARS {
        return None;
    }
    let budget = (path.len() / 4).clamp(1, 3);
    let mut best: Option<(&Candidate, usize)> = None;
    for candidate in candidates {
        if candidate.chars.len().abs_diff(path.len()) > budget {
            continue;
        }
        let cost = path.len().max(1) * (2 * budget + 1);
        if *work < cost {
            *work = 0;
            break;
        }
        *work -= cost;
        // Only a strictly nearer candidate can win, so the band shrinks.
        let limit = best.map_or(budget, |(_, b)| b.saturating_sub(1));
        if best.is_some_and(|(_, b)| b == 0) {
            break;
        }
        if let Some(d) = bounded_levenshtein(&path, &candidate.chars, limit) {
            best = Some((candidate, d));
        }
    }
    best
}

/// The edit distance between `a` and `b`, in characters, if it is at most
/// `k`; computed only within `k` of the diagonal, stopping as soon as every
/// cell of a row exceeds `k`.
fn bounded_levenshtein(a: &[char], b: &[char], k: usize) -> Option<usize> {
    const FAR: usize = usize::MAX / 2;
    if a.len().abs_diff(b.len()) > k {
        return None;
    }
    let n = b.len();
    let mut prev: Vec<usize> = (0..=n).map(|j| if j <= k { j } else { FAR }).collect();
    let mut cur = vec![FAR; n + 1];
    for (i, ca) in a.iter().enumerate() {
        let i = i + 1;
        let lo = i.saturating_sub(k).max(1);
        let hi = (i + k).min(n);
        cur[0] = if i <= k { i } else { FAR };
        if lo > 1 {
            cur[lo - 1] = FAR;
        }
        let mut row_min = cur[0];
        for j in lo..=hi {
            let cost = usize::from(*ca != b[j - 1]);
            let v = (prev[j - 1] + cost).min(prev[j] + 1).min(cur[j - 1] + 1);
            cur[j] = v;
            row_min = row_min.min(v);
        }
        if hi < n {
            cur[hi + 1] = FAR;
        }
        if row_min > k {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    (prev[n] <= k).then_some(prev[n])
}

/// A key path written the way `source` writes keys.
fn spell(segs: &[String], source: &Source, screen: &Screen<'_>) -> String {
    let dotted = segs.join(".");
    match screen {
        Screen::Env(env) => {
            let body: Vec<String> = segs
                .iter()
                .map(|s| {
                    if env.upper {
                        s.to_uppercase()
                    } else {
                        s.clone()
                    }
                })
                .collect();
            let suffix = if matches!(source, Source::SecretFile { .. }) {
                "_FILE"
            } else {
                ""
            };
            return format!("{}{}{suffix}", env.prefix, body.join("__"));
        }
        #[cfg(feature = "cli")]
        Screen::Declared => return dotted,
        Screen::All => {}
    }
    match source {
        Source::Args | Source::Cli => format!("--{dotted}"),
        Source::SecretDir(file) => Path::new(file)
            .with_file_name(&dotted)
            .display()
            .to_string(),
        _ => dotted,
    }
}

impl ConfigBuilder {
    /// Turns on unknown-key checking: every layer merged **after** this call
    /// is compared with `schema` before it is merged, and each key the schema
    /// does not know is removed from that layer and recorded as an
    /// [`UnknownKey`] with its source, its own spelling and the nearest valid
    /// key. [`build_checked`](ConfigBuilder::build_checked) then fails with
    /// all of them at once.
    ///
    /// The schema is any [`Value`] shaped like the configuration — usually
    /// `AppConfig::default().encode()`, so the valid keys come from the
    /// struct and cannot drift:
    ///
    /// - an object is a section and its entries are the valid keys;
    /// - a scalar or a list is a leaf: it accepts any scalar or list value,
    ///   but no keys below it (`port.x` is unknown where the schema has
    ///   `port: 8080`);
    /// - `null` (an `Option` that defaults to `None`) and an object with no
    ///   entries (a free-form map) accept any keys below them.
    ///
    /// `null` where the schema has a section (a YAML `engine:` heading with
    /// every key under it commented out, at any depth, or a whole layer that
    /// is `null`) means "no settings for this section": it is dropped before
    /// the merge, so the lower layers' values for the section stay in place,
    /// and it is neither reported nor recorded as the origin of anything. A
    /// section holding nothing but such null sections is dropped with them,
    /// at any depth; an explicit `{}` is still merged.
    /// `null` where the schema has `null` (an `Option` that defaults to
    /// `None`), a leaf or an empty object is merged as before, so a layer can
    /// still set an `Option` back to `None`.
    ///
    /// Any other value that is not an object where the schema has a section
    /// (`engine: 3`, `--engine=3` against an `engine` section) is not an
    /// unknown key: it is merged, replacing the section, and left for
    /// decoding to accept (an enum variant) or to report. A field whose
    /// default encodes as a scalar but which can also hold keys (an enum
    /// whose default is a unit variant) needs `null` or `{}` in the schema.
    /// An environment variable that names a section (`APP_ENGINE`) is not
    /// configuration at all and is skipped (see
    /// [`merge_env`](ConfigBuilder::merge_env)).
    ///
    /// Keys are matched exactly, so a field that decodes through an `alias`,
    /// or one left out of the encoding (`skip`, a `None` under `skip_none`),
    /// must be present in the schema to be accepted.
    ///
    /// Environment variables ([`merge_env`](ConfigBuilder::merge_env),
    /// [`merge_env_with`](ConfigBuilder::merge_env_with)) and declared
    /// command-line arguments (`merge_cli`, `merge_cli_defaults`, with the
    /// `cli` feature) whose first
    /// segment is not a top-level key of the schema (`APP_LOG`, the variables
    /// secret references point at, `--config`, `--verbose`) are not
    /// configuration: they are dropped without being reported; so are
    /// environment variables naming a section rather than a key
    /// (`APP_ENGINE`), whose `*_FILE` target is then never read. Unknown keys
    /// inside a known section are reported. Raw arguments
    /// ([`merge_args`](ConfigBuilder::merge_args)) and dynamic flags are all
    /// configuration and are always checked.
    ///
    /// Suggestions are bounded in cost: none for a path longer than 256
    /// characters, only the first 4096 schema paths are offered, and once a
    /// builder has spent its budget (a few million edit-distance steps,
    /// enough for hundreds of typical keys) later keys are still reported but
    /// get no suggestion.
    ///
    /// Without a schema nothing is checked and [`build`](ConfigBuilder::build)
    /// behaves exactly as before.
    ///
    /// ```
    /// use ferroly::codec::Value;
    /// use ferroly::config::{Config, ConfigError};
    ///
    /// let schema = Value::Object(vec![(
    ///     "engine".into(),
    ///     Value::Object(vec![("threads".into(), Value::Int(4))]),
    /// )]);
    /// let layer = Value::Object(vec![(
    ///     "engine".into(),
    ///     Value::Object(vec![("thread".into(), Value::Int(8))]),
    /// )]);
    ///
    /// let err = Config::builder()
    ///     .schema(schema)
    ///     .merge_value(layer)
    ///     .build_checked()
    ///     .unwrap_err();
    /// assert_eq!(
    ///     err.to_string(),
    ///     "unknown config key: `engine.thread` in a value layer (did you mean `engine.threads`?)"
    /// );
    /// ```
    pub fn schema(mut self, schema: Value) -> Self {
        self.check.set_schema(schema);
        self
    }

    /// Makes [`build_checked`](ConfigBuilder::build_checked) succeed despite
    /// unknown keys. They are still removed from their layer before it is
    /// merged — so an unknown key can never shadow or partly overwrite a
    /// valid one — and are listed by [`Config::ignored_keys`] for the caller
    /// to log.
    pub fn lenient(mut self) -> Self {
        self.check.lenient = true;
        self
    }

    /// Like [`build`](ConfigBuilder::build), but fails with
    /// [`ConfigError::UnknownKeys`] — listing every unknown key of every
    /// layer — when a [schema](ConfigBuilder::schema) is set, the builder is
    /// not [lenient](ConfigBuilder::lenient), and any key was unknown.
    ///
    /// `build` itself never fails: with a schema it drops the unknown keys
    /// just as lenient mode does.
    pub fn build_checked(mut self) -> Result<Config, ConfigError> {
        if !self.check.lenient && !self.check.found.is_empty() {
            return Err(ConfigError::UnknownKeys(self.check.take_found()));
        }
        Ok(self.build())
    }
}

impl Config {
    /// The keys the [schema](ConfigBuilder::schema) did not know, in the
    /// order they were found; they were left out of the configuration. Empty
    /// without a schema.
    pub fn ignored_keys(&self) -> &[UnknownKey] {
        &self.ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(paths: &[&str]) -> Vec<Candidate> {
        paths
            .iter()
            .map(|p| Candidate {
                segs: vec![p.to_string()],
                chars: p.chars().collect(),
            })
            .collect()
    }

    fn near(path: &str, candidates: &[Candidate]) -> Option<String> {
        let mut work = SUGGEST_WORK;
        suggest(path, candidates, &mut work).map(|(c, _)| c.segs[0].clone())
    }

    fn distance(a: &str, b: &str, k: usize) -> Option<usize> {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        bounded_levenshtein(&a, &b, k)
    }

    #[test]
    fn levenshtein_counts_edits_within_the_bound() {
        assert_eq!(distance("", "", 1), Some(0));
        assert_eq!(distance("abc", "", 3), Some(3));
        assert_eq!(distance("abc", "", 2), None);
        assert_eq!(distance("thread", "threads", 1), Some(1));
        assert_eq!(distance("kitten", "sitting", 3), Some(3));
        assert_eq!(distance("kitten", "sitting", 2), None);
        assert_eq!(distance("é", "e", 1), Some(1));
        assert_eq!(distance("flaw", "lawn", 2), Some(2));
        // Agrees with the full table for every pair and bound.
        let words = [
            "",
            "a",
            "ab",
            "ba",
            "abc",
            "xyz",
            "zyx",
            "flaw",
            "lawn",
            "kitten",
            "sitting",
            "abcdef",
            "badcfe",
            "server.port",
            "serve.port",
            "engine.thread",
            "engine.threads",
        ];
        for a in words {
            for b in words {
                let d = full_levenshtein(a, b);
                for k in 0..=4 {
                    let want = (d <= k).then_some(d);
                    assert_eq!(distance(a, b, k), want, "{a:?} {b:?} k={k}");
                }
            }
        }
    }

    /// The textbook table, for checking the banded one.
    fn full_levenshtein(a: &str, b: &str) -> usize {
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut cur = vec![0; b.len() + 1];
        for (i, ca) in a.chars().enumerate() {
            cur[0] = i + 1;
            for (j, cb) in b.iter().enumerate() {
                let cost = usize::from(ca != *cb);
                cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        prev[b.len()]
    }

    #[test]
    fn suggestion_budget_is_one_edit_per_four_characters() {
        let c = candidates(&["abcdefgh", "ab"]);
        // 8 characters: budget 2.
        assert_eq!(near("abcdefXX", &c).as_deref(), Some("abcdefgh"));
        assert_eq!(near("abcdeXXX", &c), None);
        // 3 characters: budget 1 (at least one).
        assert_eq!(near("abX", &c).as_deref(), Some("ab"));
        // Long keys are capped at 3.
        let long = candidates(&[&"a".repeat(40)]);
        assert!(near(&format!("{}XXX", "a".repeat(37)), &long).is_some());
        assert_eq!(near(&format!("{}XXXX", "a".repeat(36)), &long), None);
        // Equally near: the first wins; nearer: the nearer wins.
        let c = candidates(&["port1", "port2", "port"]);
        assert_eq!(near("portX", &c).as_deref(), Some("port1"));
        assert_eq!(near("port", &c).as_deref(), Some("port"));
    }

    #[test]
    fn suggestions_stop_when_the_work_is_spent() {
        let c = candidates(&["server.port"]);
        let mut work = 0;
        assert!(suggest("serve.port", &c, &mut work).is_none());
        let mut work = SUGGEST_WORK;
        assert!(suggest("serve.port", &c, &mut work).is_some());
        assert!(work < SUGGEST_WORK);
        // Too long to be worth a suggestion: no work spent.
        let long = "a".repeat(MAX_SUGGEST_CHARS + 1);
        let mut work = SUGGEST_WORK;
        assert!(suggest(&long, &candidates(&[&long]), &mut work).is_none());
        assert_eq!(work, SUGGEST_WORK);
    }

    #[test]
    fn long_keys_are_shortened_in_messages() {
        let key = UnknownKey {
            path: "k".repeat(1000),
            spelled: "k".repeat(1000),
            source: Source::Args,
            suggestion: None,
            dotted: false,
        };
        let shown = key.to_string();
        assert!(shown.len() < 300, "{shown}");
        assert!(shown.contains("(1000 chars)"), "{shown}");
    }

    fn obj(pairs: Vec<(&str, Value)>) -> Value {
        Value::Object(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// `{ model: { path, context }, port, opt: null, labels: {}, a: { b: { c } } }`
    fn env_check() -> Check {
        let mut check = Check::default();
        check.set_schema(obj(vec![
            (
                "model",
                obj(vec![
                    ("path", Value::Str(String::new())),
                    ("context", Value::Int(0)),
                ]),
            ),
            ("port", Value::Int(0)),
            ("opt", Value::Null),
            ("labels", obj(vec![])),
            ("a", obj(vec![("b", obj(vec![("c", Value::Int(1))]))])),
        ]));
        check
    }

    fn classify(check: &Check, segs: &[&str]) -> EnvKey {
        let segs: Vec<String> = segs.iter().map(|s| s.to_string()).collect();
        check.classify_env(&segs)
    }

    #[test]
    fn env_key_naming_a_section_is_ignored() {
        let check = env_check();
        assert_eq!(classify(&check, &["model"]), EnvKey::Ignore);
        assert_eq!(classify(&check, &["a"]), EnvKey::Ignore);
        assert_eq!(classify(&check, &["a", "b"]), EnvKey::Ignore);
    }

    #[test]
    fn env_key_naming_a_leaf_is_known() {
        let check = env_check();
        assert_eq!(classify(&check, &["model", "path"]), EnvKey::Known);
        assert_eq!(classify(&check, &["port"]), EnvKey::Known);
        assert_eq!(classify(&check, &["a", "b", "c"]), EnvKey::Known);
    }

    #[test]
    fn env_key_reaching_an_open_node_is_known() {
        let check = env_check();
        assert_eq!(classify(&check, &["opt"]), EnvKey::Known);
        assert_eq!(classify(&check, &["labels"]), EnvKey::Known);
        assert_eq!(classify(&check, &["labels", "anything"]), EnvKey::Known);
    }

    #[test]
    fn env_key_below_a_leaf_or_missing_is_unknown() {
        let check = env_check();
        assert_eq!(classify(&check, &["port", "x"]), EnvKey::Unknown);
        assert_eq!(classify(&check, &["model", "pth"]), EnvKey::Unknown);
        assert_eq!(classify(&check, &["log"]), EnvKey::Ignore);
    }

    #[test]
    fn env_classification_without_schema_is_known() {
        let check = Check::default();
        assert_eq!(classify(&check, &["model"]), EnvKey::Known);
        let mut check = Check::default();
        check.set_schema(obj(vec![]));
        assert_eq!(classify(&check, &["model"]), EnvKey::Known);
        let mut check = Check::default();
        check.set_schema(Value::Null);
        assert_eq!(classify(&check, &["model"]), EnvKey::Known);
    }

    #[test]
    fn strip_drops_null_where_the_schema_has_a_section() {
        let section = obj(vec![("threads", Value::Int(4))]);
        let mut out = Vec::new();
        assert_eq!(
            strip(Value::Null, &section, &mut Vec::new(), &mut out),
            None
        );
        assert!(out.is_empty(), "{out:?}");
        // Nested: the null child is dropped, the section around it kept
        // while it still holds something.
        let schema = obj(vec![(
            "server",
            obj(vec![("limits", section.clone()), ("port", Value::Int(0))]),
        )]);
        let layer = obj(vec![(
            "server",
            obj(vec![("limits", Value::Null), ("port", Value::Int(1))]),
        )]);
        assert_eq!(
            strip(layer, &schema, &mut Vec::new(), &mut out),
            Some(obj(vec![("server", obj(vec![("port", Value::Int(1))]))]))
        );
        assert!(out.is_empty(), "{out:?}");
        // A non-empty section left empty only by dropped null sections is
        // dropped too, at any depth, up to the layer itself.
        let layer = obj(vec![("server", obj(vec![("limits", Value::Null)]))]);
        assert_eq!(strip(layer, &schema, &mut Vec::new(), &mut out), None);
        assert!(out.is_empty(), "{out:?}");
        let deep = obj(vec![("a", obj(vec![("b", schema.clone())]))]);
        let layer = obj(vec![(
            "a",
            obj(vec![(
                "b",
                obj(vec![("server", obj(vec![("limits", Value::Null)]))]),
            )]),
        )]);
        assert_eq!(strip(layer, &deep, &mut Vec::new(), &mut out), None);
        assert!(out.is_empty(), "{out:?}");
        // Beside a kept sibling, only the emptied section goes.
        let schema = obj(vec![
            ("server", obj(vec![("limits", section.clone())])),
            ("port", Value::Int(0)),
        ]);
        let layer = obj(vec![
            ("server", obj(vec![("limits", Value::Null)])),
            ("port", Value::Int(1)),
        ]);
        assert_eq!(
            strip(layer, &schema, &mut Vec::new(), &mut out),
            Some(obj(vec![("port", Value::Int(1))]))
        );
        assert!(out.is_empty(), "{out:?}");
        // An explicit `{}` still passes, nested or as the whole layer.
        let layer = obj(vec![("server", obj(vec![]))]);
        assert_eq!(
            strip(layer.clone(), &schema, &mut Vec::new(), &mut out),
            Some(layer)
        );
        assert_eq!(
            strip(obj(vec![]), &schema, &mut Vec::new(), &mut out),
            Some(obj(vec![]))
        );
        // A section emptied by an unknown key is kept as before (the key is
        // reported).
        let layer = obj(vec![("server", obj(vec![("bogus", Value::Int(1))]))]);
        assert_eq!(
            strip(layer, &schema, &mut Vec::new(), &mut out),
            Some(obj(vec![("server", obj(vec![]))]))
        );
        assert_eq!(out, vec![vec!["server".to_string(), "bogus".to_string()]]);
        out.clear();
        // `null` for an `Option`, a leaf or a free-form map still passes.
        for node in [Value::Null, Value::Int(1), obj(vec![])] {
            assert_eq!(
                strip(Value::Null, &node, &mut Vec::new(), &mut out),
                Some(Value::Null)
            );
        }
        assert!(out.is_empty(), "{out:?}");
    }
}
