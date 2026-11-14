//! Simple glob matching over a [`Vfs`] tree.

use super::{Entry, Vfs, VfsError};

/// Whether `path` matches the glob `pattern`.
///
/// Both are `/`-separated and compared segment by segment, case-sensitively;
/// the match is anchored at both ends. Within a segment:
///
/// * `*` matches any run of characters (including none), but not `/`;
/// * `?` matches exactly one character, but not `/`;
/// * a segment that is exactly `**` matches any number of whole segments,
///   including none — `src/**/*.rs` matches `src/a.rs` and `src/x/y/a.rs`.
///
/// There are no character classes (`[a-z]`), no brace alternatives
/// (`{a,b}`), and no escape character; `**` inside a longer segment (`a**b`)
/// behaves like `*`. Leading, trailing and doubled slashes are ignored.
///
/// ```
/// use ferroly::vfs::glob_match;
///
/// assert!(glob_match("*.txt", "notes.txt"));
/// assert!(!glob_match("*.txt", "docs/notes.txt")); // `*` stays in one segment
/// assert!(glob_match("**/*.txt", "docs/2024/notes.txt"));
/// assert!(glob_match("docs/**", "docs/2024/notes.txt"));
/// assert!(glob_match("img-??.png", "img-07.png"));
/// ```
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let path: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pattern, &path)
}

/// Matches whole segments, with `**` as the "any number of items" wildcard.
/// Iterative with a single backtrack point, so the cost is at most
/// `pattern × path` segment comparisons — no exponential blow-up.
fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    let (mut p, mut s) = (0, 0);
    // Where the most recent `**` was, and how far into `path` it has grown.
    let mut star: Option<(usize, usize)> = None;
    while s < path.len() {
        if p < pattern.len() && pattern[p] == "**" {
            star = Some((p, s));
            p += 1;
        } else if p < pattern.len() && match_segment(pattern[p], path[s]) {
            p += 1;
            s += 1;
        } else if let Some((star_p, star_s)) = star {
            // Let the last `**` swallow one more segment and retry.
            p = star_p + 1;
            s = star_s + 1;
            star = Some((star_p, star_s + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|segment| *segment == "**")
}

/// Matches one segment against a pattern of literals, `*` and `?`, using the
/// same single-backtrack scheme over characters.
fn match_segment(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// Finds every entry under `root` whose path, **relative to `root`**, matches
/// `pattern` (see [`glob_match`] for the syntax).
///
/// Both files and directories are returned, sorted by path. The tree is
/// explored with [`Vfs::list`], descending into directories as
/// [`Vfs::walk`] would (a backend that does not report symbolic links as
/// directories is never followed through them); when the pattern has no `**`
/// the search stops at the pattern's depth. Matching is against relative
/// paths, so the same pattern works whatever the root is called.
///
/// ```
/// use ferroly::vfs::{glob, MemoryFs, Vfs};
///
/// # async fn demo() -> Result<(), ferroly::vfs::VfsError> {
/// let fs = MemoryFs::new();
/// fs.mkdir_all("/site/css").await?;
/// fs.write("/site/index.html", vec![]).await?;
/// fs.write("/site/css/main.css", vec![]).await?;
/// fs.write("/site/css/print.css", vec![]).await?;
///
/// let styles = glob(&fs, "/site", "**/*.css").await?;
/// let paths: Vec<&str> = styles.iter().map(|e| e.path.as_str()).collect();
/// assert_eq!(paths, ["/site/css/main.css", "/site/css/print.css"]);
/// # Ok(())
/// # }
/// ```
pub async fn glob(fs: &dyn Vfs, root: &str, pattern: &str) -> Result<Vec<Entry>, VfsError> {
    let segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    // Without `**`, nothing deeper than the pattern can match.
    let max_depth = if segments.contains(&"**") {
        usize::MAX
    } else {
        segments.len()
    };
    let mut matches = Vec::new();
    // (directory to list, its path relative to `root`, its depth)
    let mut pending = vec![(root.to_string(), Vec::<String>::new(), 0usize)];
    while let Some((dir, relative, depth)) = pending.pop() {
        if depth >= max_depth {
            continue;
        }
        for entry in fs.list(&dir).await? {
            let mut path = relative.clone();
            path.push(entry.name.clone());
            let parts: Vec<&str> = path.iter().map(String::as_str).collect();
            let matched = match_segments(&segments, &parts);
            if entry.is_dir {
                pending.push((entry.path.clone(), path, depth + 1));
            }
            if matched {
                matches.push(entry);
            }
        }
    }
    matches.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MemoryFs;

    #[test]
    fn single_segment_wildcards() {
        assert!(glob_match("*.txt", "a.txt"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*", "a"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(glob_match("a*b*c", "abc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
        assert!(glob_match("???", "abc"));
        assert!(!glob_match("???", "ab"));
        assert!(!glob_match("???", "abcd"));
        assert!(glob_match("file-?.log", "file-1.log"));
        assert!(!glob_match("file-?.log", "file-10.log"));
        assert!(glob_match("*é?", "caféx"));
        assert!(!glob_match("*.txt", "a.txt.bak"));
        assert!(!glob_match("A.txt", "a.txt"), "case-sensitive");
        // `*` and `?` never cross a separator.
        assert!(!glob_match("*", "a/b"));
        assert!(!glob_match("a?b", "a/b"));
        assert!(!glob_match("*.txt", "dir/a.txt"));
    }

    #[test]
    fn double_star_spans_segments() {
        assert!(glob_match("**", "a"));
        assert!(glob_match("**", "a/b/c"));
        assert!(glob_match("**/*.rs", "main.rs"));
        assert!(glob_match("**/*.rs", "src/a/b/main.rs"));
        assert!(!glob_match("**/*.rs", "src/a/b/main.rc"));
        assert!(glob_match("src/**", "src/a/b"));
        assert!(glob_match("src/**", "src"), "`**` may match nothing");
        assert!(!glob_match("src/**", "lib/a"));
        assert!(glob_match("src/**/test/*.rs", "src/test/a.rs"));
        assert!(glob_match("src/**/test/*.rs", "src/x/y/test/a.rs"));
        assert!(!glob_match("src/**/test/*.rs", "src/x/y/tests/a.rs"));
        assert!(glob_match("**/b/**/d", "a/b/c/d"));
        assert!(glob_match("**/b/**/d", "b/d"));
        assert!(glob_match("a/**/**/z", "a/z"));
        // Backtracking: the first `x` is not the one that leads to a match.
        assert!(glob_match("**/x/y", "x/q/x/y"));
        assert!(!glob_match("**/x/y", "x/q/x/z"));
    }

    #[test]
    fn slashes_and_empty_inputs() {
        assert!(glob_match("/a/b/", "a/b"));
        assert!(glob_match("a//b", "/a/b"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "a"));
        assert!(!glob_match("a", ""));
        assert!(glob_match("**", ""));
    }

    #[test]
    fn pathological_patterns_finish_quickly() {
        let pattern = "a*".repeat(40) + "b";
        let text = "a".repeat(200);
        let started = std::time::Instant::now();
        assert!(!glob_match(&pattern, &text));
        let deep_pattern = "**/".repeat(40) + "z";
        let deep_path = "a/".repeat(200) + "y";
        assert!(!glob_match(&deep_pattern, &deep_path));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn glob_matches_relative_paths() {
        let fs = MemoryFs::new();
        fs.mkdir_all("/proj/src/util").await.unwrap();
        fs.mkdir_all("/proj/docs").await.unwrap();
        for file in [
            "/proj/Cargo.toml",
            "/proj/src/lib.rs",
            "/proj/src/util/mod.rs",
            "/proj/src/util/notes.txt",
            "/proj/docs/guide.md",
        ] {
            fs.write(file, vec![]).await.unwrap();
        }
        let paths =
            |entries: Vec<Entry>| -> Vec<String> { entries.into_iter().map(|e| e.path).collect() };

        assert_eq!(
            paths(glob(&fs, "/proj", "**/*.rs").await.unwrap()),
            ["/proj/src/lib.rs", "/proj/src/util/mod.rs"]
        );
        assert_eq!(
            paths(glob(&fs, "/proj", "*.toml").await.unwrap()),
            ["/proj/Cargo.toml"]
        );
        assert_eq!(
            paths(glob(&fs, "/proj", "src/*").await.unwrap()),
            ["/proj/src/lib.rs", "/proj/src/util"]
        );
        // `**` may match no segments at all, so `src/**` includes `src`.
        assert_eq!(
            paths(glob(&fs, "/proj", "src/**").await.unwrap()),
            [
                "/proj/src",
                "/proj/src/lib.rs",
                "/proj/src/util",
                "/proj/src/util/mod.rs",
                "/proj/src/util/notes.txt"
            ]
        );
        assert_eq!(
            paths(glob(&fs, "/proj/src", "util/???.rs").await.unwrap()),
            ["/proj/src/util/mod.rs"]
        );
        assert!(glob(&fs, "/proj", "*.nope").await.unwrap().is_empty());
        assert!(matches!(
            glob(&fs, "/missing", "*").await,
            Err(VfsError::NotFound(_))
        ));
    }
}
