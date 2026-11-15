#![cfg(feature = "config")]
//! Config file discovery across directories (#95). Every environment input
//! goes through the injectable cores, so no test touches the process
//! environment.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use ferroly::config::search::{self, Platform};
use ferroly::config::{Config, ConfigError, Source};

const NAMES: [&str; 3] = ["app.yaml", "app.toml", "app.json"];

/// A fresh tree with a "current directory" and a user config directory.
fn tree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("ferroly-search-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cwd = root.join("work");
    let user = root.join("home").join(".config").join("app");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&user).unwrap();
    (root, cwd, user)
}

fn os(s: &str) -> Option<&OsStr> {
    Some(OsStr::new(s))
}

#[test]
fn directory_order_comes_before_name_order() {
    let (root, cwd, user) = tree("order");
    std::fs::write(user.join("app.yaml"), "port: 1\n").unwrap();
    std::fs::write(cwd.join("app.toml"), "port = 2\n").unwrap();
    std::fs::write(cwd.join("app.json"), "{\"port\": 3}").unwrap();

    let dirs = [cwd.clone(), user.clone()];
    assert_eq!(
        search::find_first(&dirs, &NAMES),
        Some(cwd.join("app.toml"))
    );

    // Only the user directory has a file: it is found there.
    std::fs::remove_file(cwd.join("app.toml")).unwrap();
    std::fs::remove_file(cwd.join("app.json")).unwrap();
    assert_eq!(
        search::find_first(&dirs, &NAMES),
        Some(user.join("app.yaml"))
    );

    // Nothing anywhere.
    std::fs::remove_file(user.join("app.yaml")).unwrap();
    assert_eq!(search::find_first(&dirs, &NAMES), None);
    let none: [&Path; 0] = [];
    assert_eq!(search::find_first(&none, &NAMES), None);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_directory_is_skipped_and_a_malformed_file_is_returned() {
    let (root, cwd, user) = tree("kinds");
    std::fs::create_dir_all(cwd.join("app.yaml")).unwrap();
    std::fs::write(cwd.join("app.toml"), "port = 2\n").unwrap();
    std::fs::write(user.join("app.yaml"), "port: 1\n").unwrap();
    let dirs = [cwd.clone(), user.clone()];
    assert_eq!(
        search::find_first(&dirs, &NAMES),
        Some(cwd.join("app.toml"))
    );

    // A malformed file is the answer, and fails on load instead of falling
    // through to the user directory.
    std::fs::write(cwd.join("app.toml"), "port = = =\n").unwrap();
    assert_eq!(
        search::find_first(&dirs, &NAMES),
        Some(cwd.join("app.toml"))
    );
    let err = Config::builder().merge_found(&dirs, &NAMES).unwrap_err();
    assert!(
        matches!(&err, ConfigError::Parse { path, .. } if path.ends_with("app.toml")),
        "{err:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn xdg_rule_falls_back_to_home_for_an_empty_or_relative_value() {
    let home = PathBuf::from("/home/ana");
    let xdg_dir = PathBuf::from("/xdg/config");
    let for_xdg =
        |xdg, home| search::user_config_dir_for(Platform::Xdg, "app", xdg, home, os("C:\\appdata"));

    assert_eq!(
        for_xdg(Some(xdg_dir.as_os_str()), os("/home/ana")),
        Some(xdg_dir.join("app"))
    );
    let fallback = Some(home.join(".config").join("app"));
    assert_eq!(for_xdg(None, os("/home/ana")), fallback);
    assert_eq!(for_xdg(os(""), os("/home/ana")), fallback);
    assert_eq!(for_xdg(os("relative/config"), os("/home/ana")), fallback);

    // HOME unset or empty: no user directory (an absolute XDG still works).
    assert_eq!(for_xdg(None, None), None);
    assert_eq!(for_xdg(os("relative"), None), None);
    assert_eq!(for_xdg(None, os("")), None);
    assert_eq!(
        for_xdg(Some(xdg_dir.as_os_str()), None),
        Some(xdg_dir.join("app"))
    );
}

#[test]
fn windows_rule_uses_appdata_only() {
    let appdata = "C:\\Users\\ana\\AppData\\Roaming";
    let for_win = |appdata| {
        search::user_config_dir_for(
            Platform::Windows,
            "app",
            os("/xdg"),
            os("/home/ana"),
            appdata,
        )
    };
    assert_eq!(for_win(os(appdata)), Some(Path::new(appdata).join("app")));
    assert_eq!(for_win(None), None);
    assert_eq!(for_win(os("")), None);
}

#[test]
fn the_public_entry_points_follow_this_platform() {
    let inputs = (os("/xdg"), os("/home/ana"), os("C:\\appdata"));
    assert_eq!(
        search::user_config_dir_from("app", inputs.0, inputs.1, inputs.2),
        search::user_config_dir_for(Platform::current(), "app", inputs.0, inputs.1, inputs.2)
    );
    assert_eq!(
        Platform::current(),
        if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Xdg
        }
    );

    assert_eq!(
        search::macos_native_from("app", os("/Users/ana")),
        Some(
            Path::new("/Users/ana")
                .join("Library")
                .join("Application Support")
                .join("app")
        )
    );
    assert_eq!(search::macos_native_from("app", os("")), None);
    assert_eq!(search::macos_native_from("app", None), None);

    let cwd = PathBuf::from("work");
    let user = PathBuf::from("user");
    assert_eq!(
        search::default_dirs_from(Some(cwd.clone()), Some(user.clone())),
        [cwd.clone(), user]
    );
    assert_eq!(search::default_dirs_from(Some(cwd.clone()), None), [cwd]);
    assert!(search::default_dirs_from(None, None).is_empty());

    // The real environment: the current directory always comes first.
    let dirs = search::default_dirs("app");
    assert_eq!(dirs.first(), std::env::current_dir().ok().as_ref());
}

#[test]
fn merge_found_records_the_file_in_provenance() {
    let (root, cwd, user) = tree("provenance");
    let file = user.join("app.json");
    std::fs::write(&file, r#"{"port": 9000, "db": {"password": "hunter2"}}"#).unwrap();
    let dirs = [cwd, user];

    let cfg = Config::builder()
        .merge_found(&dirs, &NAMES)
        .unwrap()
        .build();
    let shown = file.display().to_string();
    assert_eq!(cfg.get::<u16>("port").unwrap(), 9000);
    assert_eq!(cfg.source("port"), Some(&Source::File(shown.clone())));
    assert!(cfg.source("port").unwrap().is_file());
    assert_eq!(
        cfg.sources().find(|(_, s)| s.is_file()).map(|(k, _)| k),
        Some("db.password")
    );
    let dump = cfg.to_redacted_string();
    assert!(
        dump.contains(&format!("port = 9000  # file {shown}")),
        "{dump}"
    );
    assert!(!dump.contains("hunter2"), "{dump}");

    // Nothing found: the builder is unchanged.
    std::fs::remove_file(&file).unwrap();
    let cfg = Config::builder()
        .merge_found(&dirs, &NAMES)
        .unwrap()
        .build();
    assert!(cfg.sources().next().is_none());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_unusable_candidate_is_returned_instead_of_falling_through() {
    let (root, cwd, user) = tree("unusable");
    std::fs::write(user.join("app.yaml"), "port: 1\n").unwrap();

    // A dangling symlink is there, so it is the answer and fails on load
    // (CFG-8).
    #[cfg(unix)]
    {
        let dirs = [cwd.clone(), user.clone()];
        std::os::unix::fs::symlink(root.join("nowhere.yaml"), cwd.join("app.yaml")).unwrap();
        assert_eq!(
            search::find_first(&dirs, &NAMES),
            Some(cwd.join("app.yaml"))
        );
        assert!(Config::builder().merge_found(&dirs, &NAMES).is_err());
        std::fs::remove_file(cwd.join("app.yaml")).unwrap();
    }

    // A directory that may not be searched hides whether a file is there:
    // that is reported too, not skipped (unchecked when running as root, who
    // may search anything).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("app.yaml"), "port: 2\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let enforced = std::fs::metadata(locked.join("app.yaml")).is_err();
        let dirs = [locked.clone(), user.clone()];
        let found = search::find_first(&dirs, &NAMES);
        let loaded = Config::builder().merge_found(&dirs, &NAMES).is_err();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if enforced {
            assert_eq!(found, Some(locked.join("app.yaml")));
            assert!(loaded);
        }
    }

    // A directory that does not exist, or is a file, has nothing in it.
    let file_dir = root.join("plain-file");
    std::fs::write(&file_dir, "x").unwrap();
    let dirs = [root.join("missing"), file_dir, user.clone()];
    assert_eq!(
        search::find_first(&dirs, &NAMES),
        Some(user.join("app.yaml"))
    );

    // An absolute name would replace the directory: it is skipped.
    let outside = root.join("outside.yaml");
    std::fs::write(&outside, "port: 3\n").unwrap();
    let absolute = outside.display().to_string();
    assert_eq!(
        search::find_first(std::slice::from_ref(&cwd), &[absolute.as_str()]),
        None
    );
    assert_eq!(
        search::find_first(&[cwd, user.clone()], &[absolute.as_str(), "app.yaml"]),
        Some(user.join("app.yaml"))
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn the_app_name_and_home_directories_are_validated() {
    // CFG-9: `app` must be one plain name.
    for app in ["", ".", "..", "../../etc", "/etc", "a/b", "a\\b"] {
        for platform in [Platform::Xdg, Platform::Windows] {
            assert_eq!(
                search::user_config_dir_for(
                    platform,
                    app,
                    os("/xdg"),
                    os("/home/ana"),
                    os("C:\\appdata")
                ),
                None,
                "{app:?} {platform:?}"
            );
        }
        assert_eq!(search::macos_native_from(app, os("/Users/ana")), None);
    }
    assert!(search::user_config_dir_for(Platform::Xdg, "my.app", None, os("/h"), None).is_some());

    // A relative HOME or APPDATA would put the "user" directory under the
    // current directory.
    assert_eq!(
        search::user_config_dir_for(Platform::Xdg, "app", None, os("rel"), None),
        None
    );
    assert_eq!(search::macos_native_from("app", os("rel")), None);
    for appdata in ["rel\\AppData", "AppData", "C:AppData"] {
        assert_eq!(
            search::user_config_dir_for(Platform::Windows, "app", None, None, os(appdata)),
            None,
            "{appdata}"
        );
    }
    for appdata in ["C:\\Users\\ana\\AppData", "D:/data", "\\\\server\\share"] {
        assert_eq!(
            search::user_config_dir_for(Platform::Windows, "app", None, None, os(appdata)),
            Some(Path::new(appdata).join("app")),
            "{appdata}"
        );
    }
}
