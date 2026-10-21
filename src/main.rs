//! office-license-removal: Rust replacement for the postinstall script of
//! Microsoft's "Office for Mac License Removal Tool 2.7" (corrected 2.7.1-local).

mod guard;
mod registry;
mod sys;

use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

const PERP_LICENSE: &str = "Library/Preferences/com.microsoft.office.licensingV2.plist";
const PERP_BACKUP: &str = "Library/Preferences/com.microsoft.office.licensingV2.Backup";
const VNEXT_PERPETUAL: &str = "Library/Microsoft/Office/Licenses";

const GROUP_CONTAINER: &str = "Library/Group Containers/UBF8T346G9.Office";
const REGISTRY_FILE: &str = "MicrosoftRegistrationDB.reg";
const SUB_FILES: [&str; 6] = [
    "com.microsoft.Office365.plist",
    "com.microsoft.e0E2OUQxNUY1LTAxOUQtNDQwNS04QkJELTAxQTI5M0JBOTk4O.plist",
    "e0E2OUQxNUY1LTAxOUQtNDQwNS04QkJELTAxQTI5M0JBOTk4O",
    "com.microsoft.Office365V2.plist",
    "com.microsoft.O4kTOBJ0M5ITQxATLEJkQ40SNwQDNtQUOxATL1YUNxQUO2E0e.plist",
    "O4kTOBJ0M5ITQxATLEJkQ40SNwQDNtQUOxATL1YUNxQUO2E0e",
];
const SUB_LICENSES_DIR: &str = "Licenses";

const OFFICE_PREF: &str = "Library/Preferences/com.microsoft.office";
const APP_PREFS: [&str; 5] = [
    "com.microsoft.Word",
    "com.microsoft.Excel",
    "com.microsoft.Powerpoint",
    "com.microsoft.Outlook",
    "com.microsoft.onenote.mac",
];
const APP_PROCESSES: [&str; 5] = [
    "Microsoft Word",
    "Microsoft Excel",
    "Microsoft PowerPoint",
    "Microsoft Outlook",
    "Microsoft OneNote",
];

#[derive(Clone, Copy)]
enum Kind {
    Generic,
    Internet,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Generic => "generic",
            Kind::Internet => "internet",
        }
    }
}

const KEYCHAIN_ITEMS: [(Kind, &str, &str); 15] = [
    (Kind::Generic, "-s", "OneAuthAccount"),
    (Kind::Internet, "-s", "msoCredentialSchemeADAL"),
    (Kind::Internet, "-s", "msoCredentialSchemeLiveId"),
    (
        Kind::Generic,
        "-l",
        "Microsoft Office Identities Settings 2",
    ),
    (
        Kind::Generic,
        "-l",
        "Microsoft Office Identities Settings 3",
    ),
    (Kind::Generic, "-l", "Microsoft Office Identities Cache 2"),
    (Kind::Generic, "-l", "Microsoft Office Identities Cache 3"),
    (Kind::Generic, "-l", "Microsoft Office Ticket Cache"),
    (Kind::Generic, "-l", "com.microsoft.adalcache"),
    (Kind::Generic, "-l", "com.microsoft.OutlookCore.Secret"),
    (Kind::Generic, "-l", "MicrosoftOfficeRMSCredential"),
    (Kind::Generic, "-l", "MSProtection.framework.service"),
    (
        Kind::Generic,
        "-l",
        "com.helpshift.data_com.microsoft.Outlook",
    ),
    (Kind::Generic, "-G", "MSOpenTech.ADAL.1"),
    (Kind::Generic, "-G", "Microsoft Office Data"),
];
const KEYCHAIN_CAP: u32 = 20;
/// `security` exit status for "The specified item could not be found".
const SECURITY_NOT_FOUND: i32 = 44;

#[derive(Debug, PartialEq)]
enum KeychainOutcome {
    Deleted,
    NotFound,
    Failed(String),
}

/// Classify a `security find-*` / `delete-*` exit (`Deleted` = matched).
fn keychain_outcome(status: Option<i32>, stderr: &str) -> KeychainOutcome {
    let line = stderr.lines().next().unwrap_or("").trim();
    match status {
        Some(0) => KeychainOutcome::Deleted,
        Some(SECURITY_NOT_FOUND) => KeychainOutcome::NotFound,
        Some(code) if line.is_empty() => KeychainOutcome::Failed(format!("exit {code}")),
        Some(code) => KeychainOutcome::Failed(format!("exit {code}: {line}")),
        None if line.is_empty() => KeychainOutcome::Failed("no exit status (signal)".into()),
        None => KeychainOutcome::Failed(format!("no exit status: {line}")),
    }
}

#[derive(Parser)]
#[command(name = "office-license-removal", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Report which license files, keychain items, flight rows and prefs exist (read-only)
    Scan {
        /// Target user short name (default: console user)
        #[arg(long)]
        user: Option<String>,
        /// Override the target user's home directory
        #[arg(long)]
        home: Option<PathBuf>,
        /// Prefix for machine-wide paths (default: /)
        #[arg(long)]
        system_root: Option<PathBuf>,
    },
    /// Remove Office license, sign-in and flight data (root only, except lab mode)
    Remove {
        /// Target user short name (default: console user)
        #[arg(long)]
        user: Option<String>,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        skip_keychain: bool,
        #[arg(long)]
        skip_flights: bool,
        #[arg(long)]
        skip_prefs: bool,
        #[arg(long)]
        skip_quit: bool,
        /// Override the target user's home directory
        #[arg(long)]
        home: Option<PathBuf>,
        /// Prefix for machine-wide paths (default: /)
        #[arg(long)]
        system_root: Option<PathBuf>,
    },
}

struct Target {
    name: String,
    uid: u32,
    home: PathBuf,
}

struct Ctx {
    euid: u32,
    system_root: PathBuf,
    /// Ok(target) or Err(reason the per-user steps are skipped).
    target: Result<Target, String>,
    syslog: bool,
    failed: bool,
}

impl Ctx {
    fn say(&self, msg: &str) {
        println!("{msg}");
        if self.syslog {
            sys::logger(msg);
        }
    }

    fn fail(&mut self, msg: &str) {
        self.failed = true;
        self.say(msg);
    }

    fn sys_path(&self, rel: &str) -> PathBuf {
        self.system_root.join(rel)
    }

    /// Command running `prog` as the target user, in their launchd session.
    fn as_user(&self, t: &Target, prog: &str) -> Result<Command, String> {
        if self.euid == t.uid {
            Ok(Command::new(prog))
        } else if self.euid == 0 {
            let mut c = Command::new("/bin/launchctl");
            c.args([
                "asuser",
                &t.uid.to_string(),
                "/usr/bin/sudo",
                "-u",
                &t.name,
                prog,
            ]);
            Ok(c)
        } else {
            Err(format!(
                "cannot run as {} (uid {}) from uid {}; run as that user or as root",
                t.name, t.uid, self.euid
            ))
        }
    }
}

fn with_plist(p: &Path) -> PathBuf {
    let mut s: OsString = p.as_os_str().to_owned();
    s.push(".plist");
    PathBuf::from(s)
}

fn mark(found: bool) -> &'static str {
    if found {
        "[found] "
    } else {
        "[absent]"
    }
}

/// Ok(false) only for NotFound; any other error (e.g. permission denied) is
/// returned, so an unreadable path is never reported as absent.
fn present(p: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Existence check for a remove step: the guard-resolved path when present,
/// None when absent, or the message to fail with.
fn locate(path: &Path, base: &Path) -> Result<Option<PathBuf>, String> {
    match present(path) {
        Ok(true) => guard::resolve(path, base)
            .map(Some)
            .map_err(|e| format!("REFUSED {}: {e}", path.display())),
        Ok(false) => Ok(None),
        Err(e) => Err(format!("FAILED to inspect {}: {e}", path.display())),
    }
}

fn scan_line(ctx: &Ctx, what: &str, p: &Path) {
    match present(p) {
        Ok(found) => ctx.say(&format!("{} {what}: {}", mark(found), p.display())),
        Err(e) => ctx.say(&format!("[error]  {what}: {}: {e}", p.display())),
    }
}

fn clean_absolute(p: &Path) -> bool {
    p.is_absolute() && !p.components().any(|c| matches!(c, Component::ParentDir))
}

/// Validation of --system-root and --home shared by scan and remove.
fn check_overrides(system_root: &Path, home: Option<&Path>) -> Result<(), String> {
    if !clean_absolute(system_root) {
        return Err(format!(
            "--system-root '{}' must be an absolute path without '..'",
            system_root.display()
        ));
    }
    if let Some(h) = home {
        guard::check_home(h).map_err(|e| format!("--home: {e}"))?;
    }
    Ok(())
}

fn resolve_target(user: Option<String>, home_override: Option<PathBuf>) -> Result<Target, String> {
    let name = match user.or_else(sys::console_user) {
        Some(n) => n,
        None => {
            return Err("No console user; skipped per-user license, keychain, preference and flight steps (use --user <shortname> to force)".into())
        }
    };
    if !sys::valid_user_name(&name) {
        return Err(format!("Invalid user name '{name}'"));
    }
    if name == "root" {
        return Err(
            "Target user is root; skipped per-user license, keychain, preference and flight steps"
                .into(),
        );
    }
    let uid = sys::uid_of(&name).ok_or(format!("User '{name}' not found"))?;
    let home = home_override
        .or_else(|| sys::home_of(&name))
        .ok_or(format!("Could not resolve home folder for '{name}'"))?;
    let refuse = |e: String| format!("REFUSED per-user steps for '{name}': {e}");
    guard::check_home(&home).map_err(refuse)?;
    // defaults/cfprefsd silently ignore path domains reached through a symlink
    // (e.g. /var -> /private/var), so work on the real path; re-check it after.
    let home = fs::canonicalize(&home).unwrap_or(home);
    guard::check_home(&home).map_err(refuse)?;
    Ok(Target { name, uid, home })
}

/// An explicit --user that cannot be resolved is an error (None: exit 1
/// before any change); without --user, the reason only skips per-user steps.
fn target_or_exit(user: Option<String>, home: Option<PathBuf>) -> Option<Result<Target, String>> {
    let explicit = user.is_some();
    match resolve_target(user, home) {
        Err(e) if explicit => {
            eprintln!("error: --user: {e}; nothing was changed");
            None
        }
        target => Some(target),
    }
}

fn group_container(t: &Target) -> PathBuf {
    t.home.join(GROUP_CONTAINER)
}

fn app_pref(t: &Target, app: &str) -> PathBuf {
    t.home
        .join("Library/Containers")
        .join(app)
        .join("Data/Library/Preferences")
        .join(app)
}

fn keychain_cmd(
    ctx: &Ctx,
    t: &Target,
    verb: &str,
    item: (Kind, &str, &str),
) -> Result<Command, String> {
    let (kind, flag, value) = item;
    let mut c = ctx.as_user(t, "/usr/bin/security")?;
    c.args([&format!("{verb}-{}-password", kind.word()), flag, value]);
    Ok(c)
}

// ---------------------------------------------------------------- scan

fn scan(ctx: &Ctx) {
    ctx.say("Scan (read-only)");
    scan_line(ctx, "perpetual license", &ctx.sys_path(PERP_LICENSE));
    scan_line(
        ctx,
        "vNext perpetual licenses",
        &ctx.sys_path(VNEXT_PERPETUAL),
    );

    let t = match &ctx.target {
        Ok(t) => t,
        Err(reason) => {
            ctx.say(reason);
            return;
        }
    };
    ctx.say(&format!(
        "Target user: {} (uid {}); home: {}",
        t.name,
        t.uid,
        t.home.display()
    ));

    let gc = group_container(t);
    for f in SUB_FILES {
        scan_line(ctx, "subscription license", &gc.join(f));
    }
    scan_line(ctx, "subscription licenses", &gc.join(SUB_LICENSES_DIR));

    for item in KEYCHAIN_ITEMS {
        let (kind, flag, value) = item;
        match keychain_cmd(ctx, t, "find", item) {
            Ok(mut c) => {
                let (code, stderr) = sys::run(&mut c);
                let what = format!("keychain {} {flag} '{value}'", kind.word());
                ctx.say(&match keychain_outcome(code, &stderr) {
                    KeychainOutcome::Deleted => format!("{} {what}", mark(true)),
                    KeychainOutcome::NotFound => format!("{} {what}", mark(false)),
                    KeychainOutcome::Failed(why) => format!("[error]  {what}: {why}"),
                });
            }
            Err(e) => {
                ctx.say(&format!("Keychain check skipped: {e}"));
                break;
            }
        }
    }

    let reg = gc.join(REGISTRY_FILE);
    match present(&reg) {
        Ok(true) => match registry::count_flights(&reg) {
            Ok(n) => ctx.say(&format!(
                "registry: {n} flight/config value row(s) would be deleted ({})",
                reg.display()
            )),
            Err(e) => ctx.say(&format!("registry: could not read {}: {e}", reg.display())),
        },
        Ok(false) => ctx.say(&format!("{} registry: {}", mark(false), reg.display())),
        Err(e) => ctx.say(&format!("[error]  registry: {}: {e}", reg.display())),
    }

    scan_line(ctx, "pref", &with_plist(&t.home.join(OFFICE_PREF)));
    for app in APP_PREFS {
        scan_line(ctx, "pref", &with_plist(&app_pref(t, app)));
    }
}

// ---------------------------------------------------------------- remove

struct RemoveOpts {
    yes: bool,
    skip_keychain: bool,
    skip_flights: bool,
    skip_prefs: bool,
    skip_quit: bool,
    lab: bool,
}

/// Delete one path returned by `locate`. `allow_dir` mirrors rm -rf vs rm -f.
fn remove_path(path: &Path, allow_dir: bool) -> Result<(), String> {
    let md = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let res = if md.is_dir() {
        if !allow_dir {
            return Err(format!("{}: is a directory; not removed", path.display()));
        }
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    res.map_err(|e| format!("{}: {e}", path.display()))
}

fn confirm() -> bool {
    print!("Type REMOVE to continue: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(_) => line.trim_end_matches(['\r', '\n']) == "REMOVE",
        Err(_) => false,
    }
}

fn remove(ctx: &mut Ctx, o: &RemoveOpts) {
    ctx.say(&format!(
        "Starting Office for Mac License Removal (office-license-removal {})",
        env!("CARGO_PKG_VERSION")
    ));
    if o.lab {
        ctx.say("Lab mode (--home and --system-root given): skipping app quit, keychain step and cfprefsd restart");
    }

    // Quit Office apps.
    if o.lab || o.skip_quit {
        ctx.say("Skipped quitting Office apps");
    } else if let Ok(t) = &ctx.target {
        quit_apps(ctx, t.uid);
    } else {
        ctx.say("Skipped quitting Office apps: no target user");
    }

    // Perpetual / volume license: rename to .Backup.
    let perp = ctx.sys_path(PERP_LICENSE);
    match locate(&perp, &ctx.system_root) {
        Ok(Some(rp)) => match backup_perpetual(ctx, &rp) {
            Ok(backup) => ctx.say(&format!(
                "Removed {} (kept as {})",
                perp.display(),
                backup.display()
            )),
            Err(e) => ctx.fail(&format!("FAILED to remove {}: {e}", perp.display())),
        },
        Ok(None) => ctx.say(&format!("Did NOT detect {}", perp.display())),
        Err(msg) => ctx.fail(&msg),
    }

    // 2021/2024 consumer perpetual license folder.
    let vnext = ctx.sys_path(VNEXT_PERPETUAL);
    match locate(&vnext, &ctx.system_root) {
        Ok(Some(rv)) => match remove_path(&rv, true) {
            Ok(()) => ctx.say(&format!("Removed {}", vnext.display())),
            Err(e) => ctx.fail(&format!("FAILED to remove {e}")),
        },
        Ok(None) => ctx.say(&format!("Did NOT detect {}", vnext.display())),
        Err(msg) => ctx.fail(&msg),
    }

    let t = match std::mem::replace(&mut ctx.target, Err(String::new())) {
        Ok(t) => t,
        Err(reason) => {
            // A guard refusal means per-user data was left in place: exit 2.
            if reason.starts_with("REFUSED") {
                ctx.fail(&reason);
            } else {
                ctx.say(&reason);
            }
            ctx.say("Completed Office for Mac License Removal");
            return;
        }
    };
    ctx.say(&format!(
        "Per-user steps for: {} (uid {}); Home Folder: {}",
        t.name,
        t.uid,
        t.home.display()
    ));

    remove_subscription(ctx, &t);

    if o.lab || o.skip_keychain {
        ctx.say("Skipped keychain step");
    } else {
        remove_keychain(ctx, &t);
    }

    if o.skip_prefs {
        ctx.say("Skipped preference steps");
    } else {
        reset_prefs(ctx, &t);
    }

    if o.skip_flights {
        ctx.say("Skipped flight reset");
    } else {
        let reg = group_container(&t).join(REGISTRY_FILE);
        match locate(&reg, &t.home) {
            Ok(Some(rr)) => match registry::delete_flights(&rr) {
                Ok(n) => ctx.say(&format!(
                    "Removed all Office flighting data ({n} value row(s) deleted)"
                )),
                Err(e) => ctx.fail(&format!("FAILED flight reset in {}: {e}", reg.display())),
            },
            Ok(None) => ctx.say(&format!(
                "Did NOT detect {}; skipped flight reset",
                reg.display()
            )),
            Err(msg) => ctx.fail(&msg),
        }
    }

    if o.lab {
        ctx.say("Skipped cfprefsd restart");
    } else {
        let ok = sys::status(Command::new("/usr/bin/killall").arg("cfprefsd"));
        ctx.say(if ok {
            "Terminated all instances of cfprefsd"
        } else {
            "killall cfprefsd reported no process terminated"
        });
    }

    ctx.say("Completed Office for Mac License Removal");
}

/// HUP the target user's Office apps, then wait up to 5 s for them to exit.
fn quit_apps(ctx: &Ctx, uid: u32) {
    let uid = uid.to_string();
    for app in APP_PROCESSES {
        let hit = sys::status(Command::new("/usr/bin/pkill").args(["-HUP", "-u", &uid, app]));
        ctx.say(&format!(
            "Quit {app}: {}",
            if hit { "signalled" } else { "not running" }
        ));
    }
    let mut running = APP_PROCESSES.to_vec();
    for _ in 0..20 {
        running.retain(|app| sys::status(Command::new("/usr/bin/pgrep").args(["-u", &uid, app])));
        if running.is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    for app in running {
        ctx.say(&format!("WARNING: {app} still running"));
    }
}

/// Rename the located perpetual license to .Backup, or to
/// .Backup.<YYYYmmdd-HHMMSS> when an earlier backup exists; returns the
/// backup path as given (under --system-root).
fn backup_perpetual(ctx: &Ctx, perp: &Path) -> Result<PathBuf, String> {
    let mut backup = ctx.sys_path(PERP_BACKUP);
    if present(&backup).map_err(|e| format!("cannot inspect {}: {e}", backup.display()))? {
        let stamp = sys::timestamp().ok_or("could not read the date")?;
        backup = ctx.sys_path(&format!("{PERP_BACKUP}.{stamp}"));
    }
    let rb = guard::resolve(&backup, &ctx.system_root)?;
    if present(&rb).map_err(|e| format!("cannot inspect {}: {e}", rb.display()))? {
        return Err(format!("{} already exists", backup.display()));
    }
    fs::rename(perp, &rb).map_err(|e| e.to_string())?;
    Ok(backup)
}

fn remove_subscription(ctx: &mut Ctx, t: &Target) {
    let gc = group_container(t);
    let targets = SUB_FILES
        .iter()
        .map(|f| (gc.join(f), false))
        .chain([(gc.join(SUB_LICENSES_DIR), true)]);
    let mut found = false;
    let mut ok = true;
    for (p, allow_dir) in targets {
        match locate(&p, &t.home) {
            // rm -f: a path that is not there is not an error.
            Ok(None) => {}
            Ok(Some(rp)) => {
                found = true;
                match remove_path(&rp, allow_dir) {
                    Ok(()) => ctx.say(&format!("Removed {}", p.display())),
                    Err(e) => {
                        ok = false;
                        ctx.fail(&format!("FAILED to remove {e}"));
                    }
                }
            }
            Err(msg) => {
                found = true;
                ok = false;
                ctx.fail(&msg);
            }
        }
    }
    if !found {
        ctx.say("Did NOT detect any Microsoft 365 Subscription License file");
    } else if ok {
        ctx.say("Removed all Microsoft 365 Subscription License files");
    }
}

fn remove_keychain(ctx: &mut Ctx, t: &Target) {
    'items: for item in KEYCHAIN_ITEMS {
        let (kind, flag, value) = item;
        let mut n = 0;
        while n < KEYCHAIN_CAP {
            let mut c = match keychain_cmd(ctx, t, "delete", item) {
                Ok(c) => c,
                Err(e) => {
                    ctx.say(&format!("Keychain step skipped: {e}"));
                    return;
                }
            };
            let (code, stderr) = sys::run(&mut c);
            match keychain_outcome(code, &stderr) {
                KeychainOutcome::Deleted => n += 1,
                KeychainOutcome::NotFound => break,
                KeychainOutcome::Failed(why) => {
                    ctx.fail(&format!("FAILED keychain {flag} '{value}': {why}"));
                    continue 'items;
                }
            }
        }
        let cap = if n == KEYCHAIN_CAP {
            " (cap reached)"
        } else {
            ""
        };
        ctx.say(&format!(
            "Keychain: removed {n} {} item(s) matching {flag} '{value}'{cap}",
            kind.word()
        ));
    }
    ctx.say("Keychain pass complete");
}

fn reset_prefs(ctx: &mut Ctx, t: &Target) {
    // Exit code and stderr of `defaults`, run as the target user.
    let defaults =
        |ctx: &Ctx, args: &[&std::ffi::OsStr]| -> Result<(Option<i32>, String), String> {
            let mut c = ctx.as_user(t, "/usr/bin/defaults")?;
            c.args(args);
            Ok(sys::run(&mut c))
        };

    match locate(&with_plist(&t.home.join(OFFICE_PREF)), &t.home) {
        Ok(Some(plist)) => {
            // The path domain is the plist without ".plist".
            let office = plist.with_extension("");
            let d = office.as_os_str();
            let res = defaults(
                ctx,
                &[
                    "delete".as_ref(),
                    d,
                    "OfficeActivationEmailAddress".as_ref(),
                ],
            );
            match res {
                Ok((Some(0), _)) => ctx.say("Cleared OfficeActivationEmailAddress"),
                // "Could not find key ..." (macOS 27), "... does not exist" (older).
                Ok((_, err))
                    if err.contains("Could not find key") || err.contains("does not exist") =>
                {
                    ctx.say("OfficeActivationEmailAddress not present")
                }
                Ok(_) => ctx.fail(&format!(
                    "FAILED defaults delete {} OfficeActivationEmailAddress",
                    office.display()
                )),
                Err(e) => {
                    ctx.say(&format!("Preference steps skipped: {e}"));
                    return;
                }
            }
            for key in [
                "OfficeAutoSignIn",
                "HasUserSeenFREDialog",
                "HasUserSeenEnterpriseFREDialog",
            ] {
                let args: [&std::ffi::OsStr; 5] = [
                    "write".as_ref(),
                    d,
                    key.as_ref(),
                    "-bool".as_ref(),
                    "TRUE".as_ref(),
                ];
                if !matches!(defaults(ctx, &args), Ok((Some(0), _))) {
                    ctx.fail(&format!("FAILED defaults write {} {key}", office.display()));
                }
            }
        }
        Ok(None) => {}
        Err(msg) => ctx.fail(&msg),
    }

    for app in APP_PREFS {
        let key = "kSubUIAppCompletedFirstRunSetup1507";
        match locate(&with_plist(&app_pref(t, app)), &t.home) {
            Ok(Some(plist)) => {
                let p = plist.with_extension("");
                let args: [&std::ffi::OsStr; 5] = [
                    "write".as_ref(),
                    p.as_os_str(),
                    key.as_ref(),
                    "-bool".as_ref(),
                    "FALSE".as_ref(),
                ];
                match defaults(ctx, &args) {
                    Ok((Some(0), _)) => {}
                    Ok(_) => ctx.fail(&format!("FAILED defaults write {} {key}", p.display())),
                    Err(e) => {
                        ctx.say(&format!("Preference steps skipped: {e}"));
                        return;
                    }
                }
            }
            Ok(None) => {}
            Err(msg) => ctx.fail(&msg),
        }
    }
    ctx.say("Set apps back to first run state");
}

// ---------------------------------------------------------------- main

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(euid) = sys::euid() else {
        eprintln!("error: could not determine effective uid (/usr/bin/id -u)");
        return ExitCode::FAILURE;
    };

    match cli.cmd {
        Cmd::Scan {
            user,
            home,
            system_root,
        } => {
            let system_root = system_root.unwrap_or_else(|| PathBuf::from("/"));
            if let Err(e) = check_overrides(&system_root, home.as_deref()) {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
            let Some(target) = target_or_exit(user, home) else {
                return ExitCode::FAILURE;
            };
            let ctx = Ctx {
                euid,
                system_root,
                target,
                syslog: false,
                failed: false,
            };
            scan(&ctx);
            ExitCode::SUCCESS
        }
        Cmd::Remove {
            user,
            yes,
            skip_keychain,
            skip_flights,
            skip_prefs,
            skip_quit,
            home,
            system_root,
        } => {
            let lab = home.is_some() && system_root.is_some();
            if !lab && euid != 0 {
                eprintln!("error: remove must run as root (sudo), unless both --home and --system-root are given (lab mode)");
                return ExitCode::FAILURE;
            }
            let system_root = system_root.unwrap_or_else(|| PathBuf::from("/"));
            if let Err(e) = check_overrides(&system_root, home.as_deref()) {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
            let Some(target) = target_or_exit(user, home) else {
                return ExitCode::FAILURE;
            };
            let mut ctx = Ctx {
                euid,
                system_root,
                target,
                syslog: false,
                failed: false,
            };
            let opts = RemoveOpts {
                yes,
                skip_keychain,
                skip_flights,
                skip_prefs,
                skip_quit,
                lab,
            };
            if !opts.yes {
                scan(&ctx);
                if !confirm() {
                    println!("Aborted; nothing was changed.");
                    return ExitCode::FAILURE;
                }
            }
            ctx.syslog = true;
            remove(&mut ctx, &opts);
            if ctx.failed {
                eprintln!("One or more steps failed; see the messages above.");
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_outcome_classifies_exit_status() {
        use KeychainOutcome::*;
        assert_eq!(keychain_outcome(Some(0), ""), Deleted);
        assert_eq!(
            keychain_outcome(
                Some(44),
                "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.\n"
            ),
            NotFound
        );
        assert_eq!(keychain_outcome(Some(36), ""), Failed("exit 36".into()));
        assert_eq!(
            keychain_outcome(
                Some(1),
                "security: user interaction is not allowed.\nmore\n"
            ),
            Failed("exit 1: security: user interaction is not allowed.".into())
        );
        assert_eq!(
            keychain_outcome(None, ""),
            Failed("no exit status (signal)".into())
        );
    }
}
