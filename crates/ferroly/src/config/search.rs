//! Config file discovery: the first existing file among a list of
//! directories and file names.
//!
//! The usual order is the current directory, then the user's config
//! directory ([`default_dirs`]); [`find_first`] returns the first regular file
//! and [`ConfigBuilder::merge_found`] merges it.
//!
//! ```no_run
//! use ferroly::config::{search, Config};
//!
//! // ./app.yaml, ./app.toml, ./app.json, then ~/.config/app/app.yaml, …
//! let names = ["app.yaml", "app.toml", "app.json"];
//! let config = Config::builder()
//!     .merge_found(&search::default_dirs("app"), &names)?
//!     .merge_env("APP_")
//!     .build();
//! let file = config.sources().find(|(_, s)| s.is_file());
//! # let _ = file;
//! # Ok::<(), ferroly::config::ConfigError>(())
//! ```
//!
//! Every function that reads the environment has an injectable core
//! ([`user_config_dir_from`], [`user_config_dir_for`]), so each platform's
//! rule can be tested on any platform without touching the process
//! environment.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{ConfigBuilder, ConfigError};

/// Which convention locates the per-user configuration directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Platform {
    /// The XDG Base Directory rule: `$XDG_CONFIG_HOME/<app>` when that is
    /// set, non-empty and absolute, else `$HOME/.config/<app>`. Used on
    /// Linux, the BSDs and macOS (where command-line tools expect it).
    Xdg,
    /// `%APPDATA%\<app>`.
    Windows,
}

impl Platform {
    /// The convention of the platform this program was built for: Windows
    /// on Windows, XDG everywhere else.
    pub fn current() -> Self {
        if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Xdg
        }
    }
}

/// The standard per-user configuration directory for `app`:
///
/// - Linux / BSD / macOS: `$XDG_CONFIG_HOME/<app>` if that is set, non-empty
///   and absolute (as the XDG Base Directory spec requires), else
///   `$HOME/.config/<app>`; [`macos_native`] gives
///   `~/Library/Application Support/<app>` instead;
/// - Windows: `%APPDATA%\<app>`.
///
/// `None` when the needed variables are unset, empty or not absolute paths,
/// and when `app` is not one plain file name (empty, `.`, `..`, absolute, or
/// containing `/` or `\`). The directory is not checked for existence.
pub fn user_config_dir(app: &str) -> Option<PathBuf> {
    user_config_dir_from(
        app,
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("APPDATA").as_deref(),
    )
}

/// [`user_config_dir`] with the environment passed in: the values of
/// `XDG_CONFIG_HOME`, `HOME` and `APPDATA` (`None` for unset), using this
/// platform's rule ([`Platform::current`]).
pub fn user_config_dir_from(
    app: &str,
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
    appdata: Option<&OsStr>,
) -> Option<PathBuf> {
    user_config_dir_for(Platform::current(), app, xdg, home, appdata)
}

/// [`user_config_dir_from`] for an explicit [`Platform`].
pub fn user_config_dir_for(
    platform: Platform,
    app: &str,
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
    appdata: Option<&OsStr>,
) -> Option<PathBuf> {
    if !plain_name(app) {
        return None;
    }
    match platform {
        Platform::Xdg => {
            // An empty or relative XDG_CONFIG_HOME is invalid and must be
            // ignored (XDG Base Directory spec).
            let xdg = non_empty(xdg).map(Path::new).filter(|p| p.has_root());
            match xdg {
                Some(base) => Some(base.join(app)),
                None => absolute_unix(home).map(|h| h.join(".config").join(app)),
            }
        }
        Platform::Windows => absolute_windows(appdata).map(|a| a.join(app)),
    }
}

/// Whether `app` is one ordinary file name: not empty, not `.` or `..`, no
/// separator, not absolute.
fn plain_name(app: &str) -> bool {
    let mut parts = Path::new(app).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) && !app.contains(['/', '\\'])
}

/// A non-empty value that is an absolute Unix-style path (`/home/me`).
fn absolute_unix(value: Option<&OsStr>) -> Option<&Path> {
    non_empty(value).map(Path::new).filter(|p| p.has_root())
}

/// A non-empty value that is an absolute Windows path (`C:\Users\me\…`,
/// `\\server\share\…`), whichever platform this runs on.
fn absolute_windows(value: Option<&OsStr>) -> Option<&Path> {
    let value = non_empty(value)?;
    let path = Path::new(value);
    let text = value.to_string_lossy();
    let bytes = text.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let unc = text.starts_with("\\\\") || text.starts_with("//");
    (drive || unc || path.is_absolute()).then_some(path)
}

/// The macOS-native per-user directory, `$HOME/Library/Application
/// Support/<app>`, for applications that prefer it to the XDG location.
/// `None` when `HOME` is unset or empty.
pub fn macos_native(app: &str) -> Option<PathBuf> {
    macos_native_from(app, std::env::var_os("HOME").as_deref())
}

/// [`macos_native`] with the value of `HOME` passed in. `None` under the
/// same conditions as [`user_config_dir`].
pub fn macos_native_from(app: &str, home: Option<&OsStr>) -> Option<PathBuf> {
    if !plain_name(app) {
        return None;
    }
    absolute_unix(home).map(|h| h.join("Library").join("Application Support").join(app))
}

fn non_empty(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|v| !v.is_empty())
}

/// The usual search order: the current directory, then
/// [`user_config_dir`]`(app)`. Locations that cannot be determined are left
/// out.
pub fn default_dirs(app: &str) -> Vec<PathBuf> {
    default_dirs_from(std::env::current_dir().ok(), user_config_dir(app))
}

/// [`default_dirs`] with the current directory and the user config
/// directory passed in; `None` entries are left out.
pub fn default_dirs_from(cwd: Option<PathBuf>, user: Option<PathBuf>) -> Vec<PathBuf> {
    cwd.into_iter().chain(user).collect()
}

/// The first existing **regular file** among `dirs × names`: directories in
/// order, and names in order within each directory, so `./app.toml` outranks
/// `~/.config/app/app.yaml`.
///
/// Symlinks are followed; a directory (or anything else that is not a
/// regular file) with a matching name is skipped, and so is a candidate
/// whose directory does not exist. The file is not opened, so a file that
/// exists but is unreadable or malformed is returned — and reported when it
/// is loaded — rather than skipped in favour of a later candidate. The same
/// holds for a candidate that is there but cannot be checked: a dangling
/// symlink, or one in a directory that may not be searched.
///
/// `names` must be relative (`app.yaml`, `conf/app.yaml`); an absolute
/// name would replace the directory, so it is skipped.
pub fn find_first<P: AsRef<Path>>(dirs: &[P], names: &[&str]) -> Option<PathBuf> {
    let names: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| {
            let p = Path::new(name);
            !p.has_root() && !matches!(p.components().next(), Some(std::path::Component::Prefix(_)))
        })
        .collect();
    for dir in dirs {
        let dir = dir.as_ref();
        for name in &names {
            let path = dir.join(name);
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_file() => return Some(path),
                Ok(_) => {}
                // Nothing there — unless a symlink is there, pointing nowhere.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if std::fs::symlink_metadata(&path).is_ok() {
                        return Some(path);
                    }
                }
                // Something may be there but cannot be checked (permission
                // denied on the directory). If the directory itself is not a
                // directory there is nothing to find; otherwise let loading
                // report it.
                Err(_) => {
                    if std::fs::metadata(dir).map_or(true, |m| m.is_dir()) {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

impl ConfigBuilder {
    /// Merges the file [`find_first`] returns for `dirs × names`, or nothing
    /// when there is none. The format is inferred from the extension and the
    /// file is recorded in provenance ([`Source::File`](super::Source::File)),
    /// so [`Config::source`](super::Config::source) and
    /// [`Config::to_redacted_string`](super::Config::to_redacted_string) show
    /// which file was used.
    ///
    /// A found file that cannot be read or parsed is an error; the search
    /// never falls through to a later candidate.
    pub fn merge_found<P: AsRef<Path>>(
        self,
        dirs: &[P],
        names: &[&str],
    ) -> Result<Self, ConfigError> {
        match find_first(dirs, names) {
            Some(path) => self.merge_file(path),
            None => Ok(self),
        }
    }
}
