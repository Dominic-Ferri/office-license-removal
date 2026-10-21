//! End-to-end `remove` against a scratch fixture tree (lab mode only).
//! Fixture directories are kept on disk (TempDir::keep), never deleted.

use rusqlite::{params, Connection};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_office-license-removal");
const GC: &str = "Library/Group Containers/UBF8T346G9.Office";
const SUB_FILES: [&str; 6] = [
    "com.microsoft.Office365.plist",
    "com.microsoft.e0E2OUQxNUY1LTAxOUQtNDQwNS04QkJELTAxQTI5M0JBOTk4O.plist",
    "e0E2OUQxNUY1LTAxOUQtNDQwNS04QkJELTAxQTI5M0JBOTk4O",
    "com.microsoft.Office365V2.plist",
    "com.microsoft.O4kTOBJ0M5ITQxATLEJkQ40SNwQDNtQUOxATL1YUNxQUO2E0e.plist",
    "O4kTOBJ0M5ITQxATLEJkQ40SNwQDNtQUOxATL1YUNxQUO2E0e",
];
const APPS: [&str; 5] = [
    "com.microsoft.Word",
    "com.microsoft.Excel",
    "com.microsoft.Powerpoint",
    "com.microsoft.Outlook",
    "com.microsoft.onenote.mac",
];

fn current_user() -> String {
    let out = Command::new("/usr/bin/id").arg("-un").output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn euid() -> u32 {
    let out = Command::new("/usr/bin/id").arg("-u").output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn plist(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>{body}</dict></plist>\n"
        ),
    )
    .unwrap();
}

fn touch(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    reg_target: PathBuf,
}

fn fixture(tag: &str) -> Fixture {
    let base = tempfile::Builder::new()
        .prefix(&format!("olr-it-{tag}-"))
        .tempdir()
        .unwrap()
        .keep();
    // Deliberately not canonicalized: on macOS this is under the /var symlink,
    // which exercises the binary's own home canonicalization.
    eprintln!("fixture kept at {}", base.display());
    let root = base.join("root");
    let home = base.join("home");

    plist(
        &root.join("Library/Preferences/com.microsoft.office.licensingV2.plist"),
        "<key>perpetual</key><string>fixture</string>",
    );
    touch(
        &root.join("Library/Microsoft/Office/Licenses/5/licenses.json"),
        "{}",
    );
    touch(&root.join("Library/Microsoft/Office/keep.txt"), "keep");

    let gc = home.join(GC);
    for f in SUB_FILES {
        touch(&gc.join(f), "sub");
    }
    touch(&gc.join("Licenses/5/license.dat"), "lic");
    touch(&gc.join("unrelated.txt"), "keep");

    plist(
        &home.join("Library/Preferences/com.microsoft.office.plist"),
        "<key>OfficeActivationEmailAddress</key><string>fixture@example.invalid</string>",
    );
    for app in APPS {
        plist(
            &home.join(format!(
                "Library/Containers/{app}/Data/Library/Preferences/{app}.plist"
            )),
            "<key>kSubUIAppCompletedFirstRunSetup1507</key><true/>",
        );
    }

    // Registry: a symlink to the real DB file, as on current Office builds.
    let reg_target = gc.join("MicrosoftRegistrationDB/MicrosoftRegistrationDB_fixture.reg");
    fs::create_dir_all(reg_target.parent().unwrap()).unwrap();
    seed_registry(&reg_target);
    std::os::unix::fs::symlink(&reg_target, gc.join("MicrosoftRegistrationDB.reg")).unwrap();

    Fixture {
        root,
        home,
        reg_target,
    }
}

/// 10 value rows: 7 flight/config rows to delete, 3 unrelated rows to keep.
fn seed_registry(db: &Path) {
    let c = Connection::open(db).unwrap();
    c.execute_batch(
        "CREATE TABLE HKEY_CURRENT_USER (node_id INTEGER PRIMARY KEY ASC, parent_id, name TEXT COLLATE nocase, write_time, UNIQUE (parent_id, name));
         CREATE TABLE HKEY_CURRENT_USER_values (node_id INTEGER, name TEXT COLLATE nocase, type INTEGER, value, UNIQUE (node_id, name));",
    )
    .unwrap();
    for (id, parent, name) in [
        (1, -1, "Software"),
        (2, 1, "Microsoft"),
        (3, 2, "Office"),
        (4, 3, "16.0"),
        (5, 4, "Common"),
        (6, 5, "ExperimentEcs"),
        (7, 6, "excel"),
        (8, 7, "Flights"),
        (9, 5, "ExperimentConfigs"),
        (10, 9, "Ecs"),
        (11, 10, "powerpoint"),
        (12, 11, "ConfigContextData"),
        (13, 5, "General"),
    ] {
        c.execute(
            "INSERT INTO HKEY_CURRENT_USER VALUES (?1, ?2, ?3, 0)",
            params![id, parent, name],
        )
        .unwrap();
    }
    for (node, name) in [
        (7, "ETag"),
        (7, "ConfigIds"),
        (8, "SomeFlight"),
        (11, "Expires"),
        (12, "ctx"),
        (13, "Url"),
        (13, "AuthorityUrl"),
        // survivors
        (7, "Keep"),
        (13, "ProductVersion"),
        (11, "Language"),
    ] {
        c.execute(
            "INSERT INTO HKEY_CURRENT_USER_values VALUES (?1, ?2, 1, 'v')",
            params![node, name],
        )
        .unwrap();
    }
}

fn values(db: &Path) -> Vec<(i64, String)> {
    let c = Connection::open(db).unwrap();
    let mut s = c
        .prepare("SELECT node_id, name FROM HKEY_CURRENT_USER_values ORDER BY node_id, name")
        .unwrap();
    s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn defaults_read(domain: &Path, key: &str) -> Option<String> {
    // `defaults` returns {} for a path domain reached through the /var symlink.
    let real = fs::canonicalize(domain.parent().unwrap())
        .unwrap()
        .join(domain.file_name().unwrap());
    let out = Command::new("/usr/bin/defaults")
        .arg("read")
        .arg(real)
        .arg(key)
        .stderr(Stdio::null())
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn run(f: &Fixture, extra: &[&str], stdin: Option<&str>) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.arg("remove")
        .args(extra)
        .arg("--home")
        .arg(&f.home)
        .arg("--system-root")
        .arg(&f.root)
        .arg("--user")
        .arg(current_user())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    {
        let mut si = child.stdin.take().unwrap();
        if let Some(s) = stdin {
            si.write_all(s.as_bytes()).unwrap();
        }
    }
    child.wait_with_output().unwrap()
}

#[test]
fn remove_yes_cleans_fixture() {
    let f = fixture("remove");
    assert_eq!(values(&f.reg_target).len(), 10);

    let out = run(&f, &["--yes"], None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "exit {:?}", out.status.code());

    // Lab mode announced and dangerous steps skipped.
    assert!(stdout.contains("Lab mode"));
    assert!(stdout.contains("Skipped quitting Office apps"));
    assert!(stdout.contains("Skipped keychain step"));
    assert!(stdout.contains("Skipped cfprefsd restart"));
    assert!(!stdout.contains("FAILED"));

    // Perpetual plist renamed, content intact.
    let prefs = f.root.join("Library/Preferences");
    assert!(!prefs
        .join("com.microsoft.office.licensingV2.plist")
        .exists());
    let backup = prefs.join("com.microsoft.office.licensingV2.Backup");
    assert!(fs::read_to_string(&backup)
        .unwrap()
        .contains("<string>fixture</string>"));

    // vNext perpetual folder gone, its parent and sibling kept.
    assert!(!f.root.join("Library/Microsoft/Office/Licenses").exists());
    assert!(f.root.join("Library/Microsoft/Office/keep.txt").exists());

    // Subscription files and Licenses gone; unrelated file and registry kept.
    let gc = f.home.join(GC);
    for name in SUB_FILES {
        assert!(!gc.join(name).exists(), "{name} still present");
    }
    assert!(!gc.join("Licenses").exists());
    assert!(gc.join("unrelated.txt").exists());
    assert!(gc.join("MicrosoftRegistrationDB.reg").exists());

    // Prefs: files kept, keys reset through defaults.
    let office = f.home.join("Library/Preferences/com.microsoft.office");
    assert!(f
        .home
        .join("Library/Preferences/com.microsoft.office.plist")
        .is_file());
    assert_eq!(defaults_read(&office, "OfficeActivationEmailAddress"), None);
    assert_eq!(
        defaults_read(&office, "OfficeAutoSignIn").as_deref(),
        Some("1")
    );
    for app in APPS {
        let d = f.home.join(format!(
            "Library/Containers/{app}/Data/Library/Preferences/{app}"
        ));
        assert_eq!(
            defaults_read(&d, "kSubUIAppCompletedFirstRunSetup1507").as_deref(),
            Some("0"),
            "{app}"
        );
    }

    // Registry: exactly the 3 unrelated rows remain.
    assert!(stdout.contains("(7 value row(s) deleted)"));
    assert_eq!(
        values(&f.reg_target),
        vec![
            (7, "Keep".to_owned()),
            (11, "Language".to_owned()),
            (13, "ProductVersion".to_owned()),
        ]
    );
}

#[test]
fn remove_without_confirmation_aborts() {
    let f = fixture("abort");
    let out = run(&f, &[], Some("remove\n"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout.contains("Type REMOVE to continue:"));
    assert!(stdout.contains("registry: 7 flight/config value row(s) would be deleted"));
    assert!(stdout.contains("Aborted"));
    assert!(f
        .root
        .join("Library/Preferences/com.microsoft.office.licensingV2.plist")
        .exists());
    assert!(f.home.join(GC).join("Licenses").exists());
    assert_eq!(values(&f.reg_target).len(), 10);
}

#[test]
fn scan_with_overrides_reports_fixture() {
    let f = fixture("scan");
    let out = Command::new(BIN)
        .arg("scan")
        .arg("--home")
        .arg(&f.home)
        .arg("--system-root")
        .arg(&f.root)
        .arg("--user")
        .arg(current_user())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exit {:?}", out.status.code());

    let perp = f
        .root
        .join("Library/Preferences/com.microsoft.office.licensingV2.plist");
    assert!(stdout.contains(&format!("[found]  perpetual license: {}", perp.display())));
    // The binary reports the canonical home (/private/var/...).
    let lic = fs::canonicalize(&f.home).unwrap().join(GC).join("Licenses");
    assert!(stdout.contains(&format!(
        "[found]  subscription licenses: {}",
        lic.display()
    )));
    assert!(stdout.contains("registry: 7 flight/config value row(s) would be deleted"));

    // Read-only: the fixture is unchanged.
    assert!(perp.exists());
    assert!(lic.exists());
    assert_eq!(values(&f.reg_target).len(), 10);
}

#[test]
fn remove_refuses_symlinked_group_container() {
    let f = fixture("symlink");
    // Move the container out of the home (rename, not delete) and link it back.
    let gc = f.home.join(GC);
    let outside = f.home.parent().unwrap().join("outside");
    fs::rename(&gc, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &gc).unwrap();

    let out = run(&f, &["--yes"], None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("{stdout}");
    assert_eq!(out.status.code(), Some(2));
    assert!(stdout.contains("REFUSED"));
    assert!(outside.join("Licenses/5/license.dat").exists());
    for name in SUB_FILES {
        assert!(outside.join(name).exists(), "{name} deleted");
    }
    assert_eq!(values(&f.reg_target).len(), 10);
    // Machine-wide steps still ran.
    assert!(!f.root.join("Library/Microsoft/Office/Licenses").exists());
}

#[test]
fn unreadable_container_is_an_error_not_absent() {
    if euid() == 0 {
        eprintln!("running as root: skipped (root ignores mode 000)");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("perm");
    let gcs = f.home.join("Library/Group Containers");
    fs::set_permissions(&gcs, fs::Permissions::from_mode(0o000)).unwrap();
    let scan = Command::new(BIN)
        .arg("scan")
        .arg("--home")
        .arg(&f.home)
        .arg("--system-root")
        .arg(&f.root)
        .arg("--user")
        .arg(current_user())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let out = run(&f, &["--yes"], None);
    // Restore before asserting so the kept fixture stays readable.
    fs::set_permissions(&gcs, fs::Permissions::from_mode(0o755)).unwrap();

    let scan_out = String::from_utf8_lossy(&scan.stdout);
    eprintln!("{scan_out}");
    assert!(scan_out.contains("[error]  subscription licenses:"));
    assert!(scan_out.contains("[error]  registry:"));

    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("{stdout}");
    assert_eq!(out.status.code(), Some(2));
    assert!(stdout.contains("FAILED to inspect"));
    assert!(!stdout.contains("Did NOT detect any Microsoft 365"));
    assert!(f.home.join(GC).join("Licenses/5/license.dat").exists());
    assert_eq!(values(&f.reg_target).len(), 10);
}

#[test]
fn unknown_explicit_user_exits_before_any_change() {
    let f = fixture("nouser");
    let out = Command::new(BIN)
        .args(["remove", "--yes", "--user", "no-such-user-olr", "--home"])
        .arg(&f.home)
        .arg("--system-root")
        .arg(&f.root)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("User 'no-such-user-olr' not found"));
    assert!(f
        .root
        .join("Library/Preferences/com.microsoft.office.licensingV2.plist")
        .exists());
    assert!(f.root.join("Library/Microsoft/Office/Licenses").exists());
    assert!(f.home.join(GC).join("Licenses").exists());
    assert_eq!(values(&f.reg_target).len(), 10);
}

#[test]
fn existing_backup_is_not_overwritten() {
    let f = fixture("backup");
    let prefs = f.root.join("Library/Preferences");
    touch(
        &prefs.join("com.microsoft.office.licensingV2.Backup"),
        "old",
    );

    let out = run(&f, &["--yes"], None);
    assert!(out.status.success(), "exit {:?}", out.status.code());
    assert_eq!(
        fs::read_to_string(prefs.join("com.microsoft.office.licensingV2.Backup")).unwrap(),
        "old"
    );
    let stamped: Vec<PathBuf> = fs::read_dir(&prefs)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("com.microsoft.office.licensingV2.Backup.")
        })
        .collect();
    assert_eq!(stamped.len(), 1);
    assert!(fs::read_to_string(&stamped[0])
        .unwrap()
        .contains("<string>fixture</string>"));
    assert!(!prefs
        .join("com.microsoft.office.licensingV2.plist")
        .exists());
}

#[test]
fn remove_refuses_non_root_outside_lab_mode() {
    if euid() == 0 {
        eprintln!("running as root: skipped (would not be lab mode)");
        return;
    }
    // Only --home: not lab mode, so a non-root run must refuse before any step.
    let f = fixture("refuse");
    let out = Command::new(BIN)
        .args(["remove", "--yes", "--home"])
        .arg(&f.home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("must run as root"));
    assert!(f.home.join(GC).join("Licenses").exists());
}
