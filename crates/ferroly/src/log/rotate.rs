//! A rotating log file: size-based and optional daily rotation with a bounded
//! number of retained files. Standard library only.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// When a [`RotatingFile`] rotates and how many old files it keeps.
///
/// ```
/// use ferroly::log::Rotation;
///
/// // Rotate at 50 MiB or at UTC midnight, keep the 7 most recent old files.
/// let policy = Rotation::new().max_bytes(50 * 1024 * 1024).daily(true).keep(7);
/// # let _ = policy;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotation {
    max_bytes: u64,
    keep: usize,
    daily: bool,
}

impl Default for Rotation {
    /// Rotate at 10 MiB, keep 5 old files, no daily rotation.
    fn default() -> Self {
        Rotation {
            max_bytes: 10 * 1024 * 1024,
            keep: 5,
            daily: false,
        }
    }
}

impl Rotation {
    /// The default policy: rotate at 10 MiB, keep 5 old files, no daily
    /// rotation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rotates before a write that would take the file past `bytes`. `0`
    /// disables size-based rotation. A single record larger than the limit is
    /// still written whole, into a file of its own.
    pub fn max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// Keeps at most `files` rotated files (`app.log.1` … `app.log.<files>`);
    /// the oldest is deleted on rotation. `0` keeps none: rotation simply
    /// truncates the live file.
    pub fn keep(mut self, files: usize) -> Self {
        self.keep = files;
        self
    }

    /// Also rotates when the UTC calendar day changes (checked on each write).
    pub fn daily(mut self, daily: bool) -> Self {
        self.daily = daily;
        self
    }
}

/// A `Write` implementation over a log file that rotates itself.
///
/// On rotation the live file `app.log` becomes `app.log.1`, the previous
/// `app.log.1` becomes `app.log.2`, and so on; the file beyond the
/// [`keep`](Rotation::keep) count is deleted, and a fresh `app.log` is started.
/// Higher numbers are older.
///
/// Each `write` call is written whole, into one file — the logger hands a
/// complete record (with its newline) to a single call, so a record is never
/// split across two files. The file is opened in append mode and writes are
/// unbuffered (one `write` system call per record); wrap the logger in a
/// [non-blocking sink](super::Sink::non_blocking) to move that I/O off the
/// calling thread.
///
/// ```no_run
/// use ferroly::log::{Logger, RotatingFile, Rotation};
///
/// # fn main() -> std::io::Result<()> {
/// let file = RotatingFile::open("/var/log/app/app.log", Rotation::new().keep(7))?;
/// let log = Logger::json().async_to(file, 8192);
/// log.info("started", &[]);
/// log.flush();
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct RotatingFile {
    path: PathBuf,
    policy: Rotation,
    file: Option<File>,
    size: u64,
    /// The UTC day (days since the epoch) the live file belongs to.
    day: i64,
}

fn day_of(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() / 86_400) as i64,
        Err(_) => 0,
    }
}

impl RotatingFile {
    /// Opens (creating if needed, appending if present) the log file at `path`.
    /// The parent directory must exist. With daily rotation, a file last
    /// modified on an earlier day is rotated on the first write.
    pub fn open(path: impl Into<PathBuf>, policy: Rotation) -> io::Result<Self> {
        let path = path.into();
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let meta = file.metadata()?;
        let size = meta.len();
        let today = day_of(SystemTime::now());
        // An empty file has no history worth dating; otherwise trust its mtime.
        let day = if size == 0 {
            today
        } else {
            meta.modified().map(day_of).unwrap_or(today)
        };
        Ok(RotatingFile {
            path,
            policy,
            file: Some(file),
            size,
            day,
        })
    }

    /// The path of the live file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The path of the `n`-th rotated file (`app.log.<n>`).
    fn rotated(&self, n: usize) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    /// Rotates now, regardless of size or date.
    pub fn rotate(&mut self) -> io::Result<()> {
        self.rotate_on(day_of(SystemTime::now()))
    }

    fn rotate_on(&mut self, today: i64) -> io::Result<()> {
        // Close the live file before renaming it (required on Windows).
        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
        }
        let keep = self.policy.keep;
        if keep == 0 {
            match fs::remove_file(&self.path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        } else {
            // Drop the oldest, then shift every file up by one. A missing link
            // in the chain is not an error.
            let _ = fs::remove_file(self.rotated(keep));
            for n in (1..keep).rev() {
                let from = self.rotated(n);
                if from.exists() {
                    fs::rename(&from, self.rotated(n + 1))?;
                }
            }
            if self.path.exists() {
                fs::rename(&self.path, self.rotated(1))?;
            }
        }
        self.reopen(today)
    }

    fn reopen(&mut self, today: i64) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = file.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = Some(file);
        self.day = today;
        Ok(())
    }

    /// Writes `buf` whole, rotating first when the policy asks for it.
    fn write_on(&mut self, buf: &[u8], today: i64) -> io::Result<usize> {
        let day_changed = self.policy.daily && today != self.day;
        let too_big = self.policy.max_bytes > 0
            && self.size > 0
            && self.size.saturating_add(buf.len() as u64) > self.policy.max_bytes;
        if day_changed || too_big {
            self.rotate_on(today)?;
        }
        if self.file.is_none() {
            // A previous rotation failed part-way; try to recover.
            self.reopen(today)?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is not open"))?;
        file.write_all(buf)?;
        self.size += buf.len() as u64;
        Ok(buf.len())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_on(buf, day_of(SystemTime::now()))
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A unique, empty scratch directory under the system temp dir.
    fn scratch(tag: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ferroly-log-rotate-{}-{}-{}",
            std::process::id(),
            tag,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn daily_rotation_follows_the_day_number() {
        let dir = scratch("daily");
        let path = dir.join("app.log");
        let policy = Rotation::new().max_bytes(0).daily(true).keep(2);
        let mut f = RotatingFile::open(&path, policy).unwrap();
        let day = f.day;

        f.write_on(b"day0-a\n", day).unwrap();
        f.write_on(b"day0-b\n", day).unwrap();
        assert!(!f.rotated(1).exists(), "same day: no rotation");

        f.write_on(b"day1\n", day + 1).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "day1\n");
        assert_eq!(
            fs::read_to_string(f.rotated(1)).unwrap(),
            "day0-a\nday0-b\n"
        );

        f.write_on(b"day2\n", day + 2).unwrap();
        f.write_on(b"day3\n", day + 3).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "day3\n");
        assert_eq!(fs::read_to_string(f.rotated(1)).unwrap(), "day2\n");
        assert_eq!(fs::read_to_string(f.rotated(2)).unwrap(), "day1\n");
        assert!(!f.rotated(3).exists(), "only `keep` old files remain");

        drop(f);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_old_file_is_rotated_on_the_first_write_of_a_new_day() {
        let dir = scratch("stale");
        let path = dir.join("app.log");
        fs::write(&path, "yesterday\n").unwrap();
        let mut f = RotatingFile::open(&path, Rotation::new().daily(true)).unwrap();
        // The file's own date is taken from its modification time (today).
        let opened_day = f.day;
        f.write_on(b"today\n", opened_day + 1).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "today\n");
        assert_eq!(fs::read_to_string(f.rotated(1)).unwrap(), "yesterday\n");
        drop(f);
        fs::remove_dir_all(&dir).unwrap();
    }
}
