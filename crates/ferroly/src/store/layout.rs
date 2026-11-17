//! Where committed objects live under a store's root.

use std::path::{Component, Path, PathBuf};

use super::local::{LOCK, OBJECTS, PARTIAL, TMP};
use super::{Digest, StoreError};

/// Where committed objects live under the store root.
///
/// Temporary and partial files always live in `<root>/tmp` and
/// `<root>/partial` (on the same filesystem as the objects, so committing
/// stays an atomic rename), and the store lock is `<root>/.lock`, whatever
/// the layout.
///
/// ```
/// use ferroly::store::Layout;
///
/// assert_eq!(Layout::default(), Layout::Fanout);
/// let flat = Layout::flat("blobs").unwrap(); // <root>/blobs/sha256/<hex>
/// assert!(Layout::flat("../outside").is_err());
/// assert!(Layout::flat("tmp").is_err()); // the store's own directory
/// # let _ = flat;
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Layout {
    /// `<root>/sha256/<first 2 hex>/<64 hex>`: objects spread over up to
    /// 256 fan-out directories. The default, and the layout of every store
    /// created before layouts existed.
    #[default]
    Fanout,
    /// `<root>/<prefix>/sha256/<64 hex>`, one flat directory (for example
    /// `prefix = "blobs"` gives `blobs/sha256/<hex>`). Build it with
    /// [`Layout::flat`], which validates the prefix.
    Flat {
        /// The directory, relative to the root, that holds `sha256/`.
        prefix: PathBuf,
    },
}

impl Layout {
    /// The flat variant, `<root>/<prefix>/sha256/<hex>`.
    ///
    /// Fails with [`StoreError::Config`] when `prefix` is empty, absolute,
    /// contains `..` or a root or drive component, has a component ending in
    /// `.` or a space (Windows drops those, so `tmp.` would name `tmp`), or
    /// starts with `tmp`, `partial` or `.lock` (the store's own entries;
    /// compared ignoring ASCII case, for case-insensitive filesystems). `.`
    /// components are dropped.
    pub fn flat(prefix: impl Into<PathBuf>) -> Result<Layout, StoreError> {
        Ok(Layout::Flat {
            prefix: clean_prefix(&prefix.into())?,
        })
    }

    /// Checks a layout built directly (a `Flat` that did not go through
    /// [`Layout::flat`]) and returns it in its normalised form.
    pub(crate) fn validated(self) -> Result<Layout, StoreError> {
        match self {
            Layout::Fanout => Ok(Layout::Fanout),
            Layout::Flat { prefix } => Layout::flat(prefix),
        }
    }

    /// The directory holding the objects, relative to the root (`sha256` or
    /// `<prefix>/sha256`).
    pub(crate) fn objects_dir(&self) -> PathBuf {
        match self {
            Layout::Fanout => PathBuf::from(OBJECTS),
            Layout::Flat { prefix } => prefix.join(OBJECTS),
        }
    }

    /// The path of the object for `digest`, relative to the root.
    pub(crate) fn object_path(&self, digest: &Digest) -> PathBuf {
        let hex = digest.hex();
        match self {
            Layout::Fanout => Path::new(OBJECTS).join(&hex[..2]).join(hex),
            Layout::Flat { prefix } => prefix.join(OBJECTS).join(hex),
        }
    }
}

/// Validates a flat-layout prefix and drops its `.` components.
fn clean_prefix(prefix: &Path) -> Result<PathBuf, StoreError> {
    let bad = |why: &str| {
        let shown: String = prefix.display().to_string().chars().take(80).collect();
        StoreError::Config(format!("invalid layout prefix {shown:?}: {why}"))
    };
    let mut out = PathBuf::new();
    for component in prefix.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                // Win32 strips trailing dots and spaces from every name, so
                // `tmp.` or `.lock ` would alias the store's own entries
                // there. Refused everywhere, so a prefix names the same
                // directory on every platform.
                let text = part.to_string_lossy();
                if text.ends_with('.') || text.ends_with(' ') {
                    return Err(bad("a component may not end in `.` or a space"));
                }
                out.push(part)
            }
            Component::ParentDir => return Err(bad("it may not contain `..`")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(bad("it must be a relative path"))
            }
        }
    }
    let Some(first) = out.components().next() else {
        return Err(bad("it is empty"));
    };
    let first = first.as_os_str().to_string_lossy();
    if [TMP, PARTIAL, LOCK]
        .iter()
        .any(|own| first.eq_ignore_ascii_case(own))
    {
        return Err(bad("it collides with the store's own entries"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fanout_paths() {
        let d = Digest::of(b"abc");
        let hex = d.hex();
        assert_eq!(Layout::Fanout.objects_dir(), PathBuf::from("sha256"));
        assert_eq!(
            Layout::Fanout.object_path(&d),
            Path::new("sha256").join(&hex[..2]).join(&hex)
        );
    }

    #[test]
    fn flat_paths() {
        let d = Digest::of(b"abc");
        let flat = Layout::flat("blobs").unwrap();
        assert_eq!(flat.objects_dir(), Path::new("blobs").join("sha256"));
        assert_eq!(
            flat.object_path(&d),
            Path::new("blobs").join("sha256").join(d.hex())
        );
        let nested = Layout::flat("./a/./b").unwrap();
        assert_eq!(
            nested,
            Layout::Flat {
                prefix: Path::new("a").join("b")
            }
        );
    }

    #[test]
    fn bad_prefixes_are_rejected() {
        for bad in [
            "",
            ".",
            "./.",
            "/abs",
            "../x",
            "a/../../b",
            "a/..",
            "tmp",
            "partial",
            ".lock",
            "TMP",
            "tmp/x",
            "./partial",
            // Windows strips trailing dots and spaces: the first three name
            // `tmp`, `partial` and `.lock` there.
            "tmp.",
            "partial ",
            ".lock.",
            "TMP. .",
            "blobs/x.",
            "a /b",
            "...",
        ] {
            let err = Layout::flat(bad).unwrap_err();
            assert!(matches!(err, StoreError::Config(_)), "{bad:?}: {err:?}");
        }
        for good in [
            "blobs", "a/b", "sha256", "tmpx", "x/tmp", ".locks", "a.b", " a", "./b",
        ] {
            assert!(Layout::flat(good).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn directly_built_layouts_are_validated() {
        let raw = Layout::Flat {
            prefix: PathBuf::from("../escape"),
        };
        assert!(raw.validated().is_err());
        let raw = Layout::Flat {
            prefix: PathBuf::from("./blobs"),
        };
        assert_eq!(raw.validated().unwrap(), Layout::flat("blobs").unwrap());
        assert_eq!(Layout::Fanout.validated().unwrap(), Layout::Fanout);
    }

    #[test]
    fn long_prefixes_are_not_echoed_in_full() {
        let long = format!("../{}", "a".repeat(10_000));
        let err = Layout::flat(long).unwrap_err().to_string();
        assert!(err.len() < 300, "{}", err.len());
    }
}
