#![cfg(feature = "fsutils")]
//! `FileLock` across processes: a second process is kept out while the holder
//! lives, and gets the lock once the holder is killed, without any clean-up.
//! The create-new `LockFile`, by contrast, stays held by a dead holder.
//!
//! The holder is this test binary itself, re-run with an environment variable
//! that turns `child_holds_the_locks` into the holding process.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use ferroly::fsutils::{FileLock, LockFile, TempDir};

const CHILD_ENV: &str = "FERROLY_FILE_LOCK_CHILD_DIR";

/// In the child: take both locks, announce it, and wait to be killed. In a
/// normal test run (no environment variable) it does nothing.
#[test]
fn child_holds_the_locks() {
    let Some(dir) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let os = FileLock::acquire(dir.join("os.lock")).unwrap();
    let file = LockFile::acquire(dir.join("file.lock")).unwrap();
    println!("locked");
    std::thread::sleep(Duration::from_secs(120));
    drop((os, file));
}

#[test]
fn a_killed_holder_releases_the_os_lock() {
    let dir = TempDir::new("ferroly-file-lock-xproc").unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["child_holds_the_locks", "--exact", "--nocapture"])
        .env(CHILD_ENV, dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    loop {
        let line = lines
            .next()
            .expect("the child exited before taking the locks")
            .unwrap();
        if line.trim() == "locked" {
            break;
        }
    }

    let os = dir.path().join("os.lock");
    let file = dir.path().join("file.lock");
    // While the child lives, both locks keep this process out.
    assert!(FileLock::try_acquire(&os).unwrap().is_none());
    assert!(LockFile::try_acquire(&file).unwrap().is_none());
    assert_eq!(FileLock::owner(&os).unwrap(), Some(child.id()));

    child.kill().unwrap();
    child.wait().unwrap();

    // The kernel released the OS lock; nobody had to break anything.
    let lock = FileLock::acquire_timeout(&os, Duration::from_secs(10))
        .expect("a dead holder must not block a FileLock");
    assert_eq!(FileLock::owner(&os).unwrap(), Some(std::process::id()));
    drop(lock);
    // The create-new lock file is left behind and still looks held.
    assert!(LockFile::try_acquire(&file).unwrap().is_none());
}
