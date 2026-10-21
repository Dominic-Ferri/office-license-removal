//! Thin wrappers around the macOS command-line tools the script used.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Run a command with stdout/stderr discarded; true on exit status 0.
pub fn status(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a command with stdout discarded; returns its exit code (None when it
/// was killed by a signal or could not start) and its stderr.
pub fn run(cmd: &mut Command) -> (Option<i32>, String) {
    match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) => (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Err(e) => (None, e.to_string()),
    }
}

/// Local time as YYYYmmdd-HHMMSS.
pub fn timestamp() -> Option<String> {
    output(Command::new("/bin/date").arg("+%Y%m%d-%H%M%S")).map(|s| s.trim().to_owned())
}

/// Run a command and return its stdout when it exits 0.
fn output(cmd: &mut Command) -> Option<String> {
    let out = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_uid(s: &str) -> Option<u32> {
    s.trim().parse().ok()
}

/// Effective uid of this process (`id -u` reports the effective uid).
pub fn euid() -> Option<u32> {
    parse_uid(&output(Command::new("/usr/bin/id").arg("-u"))?)
}

pub fn uid_of(user: &str) -> Option<u32> {
    parse_uid(&output(Command::new("/usr/bin/id").args(["-u", user]))?)
}

/// Short name of the user that runs this process (`id -un`).
pub fn user_name() -> Option<String> {
    output(Command::new("/usr/bin/id").arg("-un")).map(|s| s.trim().to_owned())
}

/// Console user from `scutil`, ignoring `loginwindow` (script: GetLoggedInUser).
pub fn console_user() -> Option<String> {
    let mut child = Command::new("/usr/sbin/scutil")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()?
        .write_all(b"show State:/Users/ConsoleUser\n")
        .ok()?;
    let out = child.wait_with_output().ok()?;
    parse_console_user(&String::from_utf8_lossy(&out.stdout))
}

fn parse_console_user(text: &str) -> Option<String> {
    text.lines()
        .filter(|l| l.contains("Name :") && !l.contains("loginwindow"))
        .find_map(|l| l.split_whitespace().nth(2))
        .map(str::to_owned)
}

/// NFSHomeDirectory from dscl, else /Users/<user> when that directory exists.
pub fn home_of(user: &str) -> Option<PathBuf> {
    let from_dscl = output(Command::new("/usr/bin/dscl").args([
        ".",
        "-read",
        &format!("/Users/{user}"),
        "NFSHomeDirectory",
    ]))
    .and_then(|s| {
        s.split_once("NFSHomeDirectory:")
            .map(|(_, rest)| rest.trim().to_owned())
    })
    .filter(|s| !s.is_empty());
    match from_dscl {
        Some(h) => Some(PathBuf::from(h)),
        None => {
            let fallback = PathBuf::from(format!("/Users/{user}"));
            fallback.is_dir().then_some(fallback)
        }
    }
}

/// A macOS short name that is safe to pass as an argument to id/sudo/dscl.
pub fn valid_user_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub fn logger(msg: &str) {
    status(Command::new("/usr/bin/logger").args(["-t", "OfficeLicenseRemoval", msg]));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_user_parsing() {
        let sample = "<dictionary> {\n  GID : 20\n  Name : alice\n  SessionInfo : <array> {\n    0 : <dictionary> {\n      kCGSSessionUserNameKey : alice\n    }\n  }\n  UID : 501\n}\n";
        assert_eq!(parse_console_user(sample).as_deref(), Some("alice"));
        assert_eq!(
            parse_console_user("<dictionary> {\n  Name : loginwindow\n}\n"),
            None
        );
        assert_eq!(parse_console_user("  No such key\n"), None);
    }

    #[test]
    fn user_name_validation() {
        assert!(valid_user_name("dominic.ferri"));
        assert!(valid_user_name("a_b-1"));
        assert!(!valid_user_name(""));
        assert!(!valid_user_name("-u"));
        assert!(!valid_user_name("bob; rm"));
        assert!(!valid_user_name("../x"));
    }
}
