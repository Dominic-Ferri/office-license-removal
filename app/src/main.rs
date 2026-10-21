//! Native window for the `office-license-removal` CLI that sits next to this
//! executable (`Contents/MacOS/` in the app bundle).

use std::cell::RefCell;
use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertSecondButtonReturn, NSAlertStyle, NSAppearance, NSAppearanceCustomization,
    NSAppearanceNameDarkAqua, NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSAutoresizingMaskOptions, NSBackingStoreType, NSBorderType,
    NSBox, NSBoxType, NSButton, NSColor, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSMenu, NSMenuItem, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSTextAlignment, NSTextField, NSTextView, NSTitlePosition,
    NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{
    MainThreadMarker, NSAttributedString, NSDictionary, NSNotification, NSObjectProtocol, NSPoint,
    NSRange, NSRect, NSSize, NSString, NSTimer,
};

const TITLE: &str = "Office License Removal";
const W: f64 = 760.0;
const H: f64 = 540.0;
const BAND: f64 = 80.0;
const EXIT_MARKER: &str = "OLR_EXIT=";
const CANCELLED: &str = "Cancelled; nothing was changed.";
const WAIT: &str = "Wait for the current run to finish.";

mod uitest;

#[derive(Clone)]
struct Config {
    cli: String,
    user: String,
    /// (OLR_LAB_HOME, OLR_LAB_SYSTEM_ROOT) when both are set.
    lab: Option<(OsString, OsString)>,
}

impl Config {
    fn load() -> Result<Config, String> {
        let exe = std::env::current_exe().map_err(|e| format!("cannot locate this app: {e}"))?;
        let cli = exe.with_file_name("office-license-removal");
        if !cli.is_file() {
            return Err(format!(
                "The office-license-removal tool is missing. Expected it at {}",
                cli.display()
            ));
        }
        // Fail closed: the path is shell-quoted for the elevated run, so it must be exact.
        let cli = cli
            .to_str()
            .ok_or(format!("{} is not a UTF-8 path", cli.display()))?
            .to_owned();
        let out = Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .map_err(|e| format!("/usr/bin/id -un: {e}"))?;
        let user = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !out.status.success() || user.is_empty() {
            return Err("Could not determine the current user (/usr/bin/id -un)".into());
        }
        let lab = match (
            std::env::var_os("OLR_LAB_HOME"),
            std::env::var_os("OLR_LAB_SYSTEM_ROOT"),
        ) {
            (Some(h), Some(r)) => Some((h, r)),
            _ => None,
        };
        Ok(Config { cli, user, lab })
    }

    fn args(&self, sub: &str) -> Vec<OsString> {
        let mut a: Vec<OsString> = vec![sub.into(), "--user".into(), self.user.clone().into()];
        if let Some((home, root)) = &self.lab {
            a.extend([
                "--home".into(),
                home.clone(),
                "--system-root".into(),
                root.clone(),
            ]);
        }
        a
    }
}

/// POSIX shell single-quoting, as AppleScript's `quoted form of`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

/// SHA-256 of `path` as 64 hex digits, from `/usr/bin/shasum`.
fn sha256_hex(path: &str) -> Result<String, String> {
    let out = Command::new("/usr/bin/shasum")
        .args(["-a", "256", path])
        .output()
        .map_err(|e| format!("/usr/bin/shasum: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let hash = text.split(' ').next().unwrap_or_default();
    // Fail closed: the hash is interpolated into the elevated shell command.
    if out.status.success() && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(hash.to_owned())
    } else {
        Err(format!("could not compute the SHA-256 of {path}"))
    }
}

/// Root must not run the CLI from a folder the user can write to, so the
/// elevated shell copies it into a new root-only temp dir (left in place),
/// checks the copy against the hash taken at confirmation, and runs the copy.
/// `do shell script` turns a non-zero exit into an AppleScript error, which
/// hides the CLI's own exit code, so the shell appends it as a marker line.
fn elevated_script(cli: &str, user: &str, sha256: &str) -> String {
    let cmd = format!(
        "( t=$(/usr/bin/mktemp -d /private/tmp/olr.XXXXXX) || exit 1; \
         /bin/cp {} \"$t/office-license-removal\" || exit 1; \
         h=$(/usr/bin/shasum -a 256 \"$t/office-license-removal\"); \
         [ \"${{h%% *}}\" = {} ] || {{ echo 'REFUSED: office-license-removal changed since confirmation'; exit 3; }}; \
         exec \"$t/office-license-removal\" remove --yes --user {} ) 2>&1; echo {EXIT_MARKER}$?",
        sh_quote(cli),
        sh_quote(sha256),
        sh_quote(user)
    );
    format!(
        "do shell script {} with administrator privileges without altering line endings",
        applescript_string(&cmd)
    )
}

/// Split the trailing exit-code marker off the elevated run's stdout.
fn split_exit_marker(out: &str) -> (String, Option<i32>) {
    match out.trim_end().rsplit_once(EXIT_MARKER) {
        Some((body, code)) => (body.to_owned(), code.trim().parse().ok()),
        None => (out.to_owned(), None),
    }
}

#[derive(Clone)]
enum Job {
    Scan,
    ScanAfterRemove,
    /// SHA-256 of the CLI, taken when the user confirmed.
    Remove(String),
}

enum Outcome {
    /// `code` is None when the process was killed by a signal.
    Exited {
        code: Option<i32>,
        output: String,
    },
    PasswordCancelled,
    NotStarted(String),
}

fn exit_text(code: Option<i32>) -> String {
    code.map_or_else(|| "signal".to_owned(), |c| c.to_string())
}

/// Run a command with stdout and stderr on one pipe, so the output keeps its order.
fn run_combined(mut cmd: Command) -> std::io::Result<(Option<i32>, String)> {
    let (mut reader, writer) = std::io::pipe()?;
    cmd.stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    let mut child = cmd.spawn()?;
    // `cmd` holds the parent's copies of the write end; without dropping it
    // the read below never sees end-of-file.
    drop(cmd);
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;
    let status = child.wait()?;
    Ok((status.code(), String::from_utf8_lossy(&buf).into_owned()))
}

fn execute(cfg: &Config, job: Job) -> Outcome {
    let direct = |sub: &str, extra: &[&str]| {
        let mut cmd = Command::new(&cfg.cli);
        cmd.args(cfg.args(sub)).args(extra);
        match run_combined(cmd) {
            Ok((code, output)) => Outcome::Exited { code, output },
            Err(e) => Outcome::NotStarted(format!("{}: {e}", cfg.cli)),
        }
    };
    match job {
        Job::Scan | Job::ScanAfterRemove => direct("scan", &[]),
        Job::Remove(_) if cfg.lab.is_some() => direct("remove", &["--yes"]),
        Job::Remove(sha256) => {
            let out = match Command::new("/usr/bin/osascript")
                .arg("-e")
                .arg(elevated_script(&cfg.cli, &cfg.user, &sha256))
                .stdin(Stdio::null())
                .output()
            {
                Ok(o) => o,
                Err(e) => return Outcome::NotStarted(format!("/usr/bin/osascript: {e}")),
            };
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if out.status.success() {
                let (output, code) = split_exit_marker(&stdout);
                Outcome::Exited { code, output }
            } else if stderr.contains("(-128)") {
                Outcome::PasswordCancelled
            } else {
                Outcome::Exited {
                    code: out.status.code(),
                    output: format!("{stdout}{stderr}"),
                }
            }
        }
    }
}

struct Running {
    job: Job,
    rx: Receiver<Outcome>,
    timer: Retained<NSTimer>,
}

struct Ivars {
    config: Result<Config, String>,
    window: Retained<NSWindow>,
    status: Retained<NSTextField>,
    log_view: Retained<NSTextView>,
    rescan: Retained<NSButton>,
    remove: Retained<NSButton>,
    log: RefCell<String>,
    running: RefCell<Option<Running>>,
    /// The status that the quit refusal message replaced.
    status_before_wait: RefCell<String>,
    /// The confirmation alert while it is on screen (read by the UI test harness).
    alert: RefCell<Option<Retained<NSAlert>>>,
    ui: Option<uitest::Harness>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Controller does
    // not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    struct Controller;

    // SAFETY (each `#[unsafe(method(...))]` below): the Rust signature
    // matches the selector: action methods take one object argument and
    // return void; the delegate methods match NSApplicationDelegate.
    impl Controller {
        #[unsafe(method(rescan:))]
        fn rescan(&self, _sender: Option<&AnyObject>) {
            self.start(Job::Scan);
        }

        #[unsafe(method(removeLicenses:))]
        fn remove_licenses(&self, _sender: Option<&AnyObject>) {
            self.confirm_and_remove();
        }

        #[unsafe(method(poll:))]
        fn poll_timer(&self, _timer: Option<&AnyObject>) {
            self.poll();
        }

        #[unsafe(method(uiTick:))]
        fn ui_tick(&self, _timer: Option<&AnyObject>) {
            uitest::tick(self);
        }

        #[unsafe(method(uiOpenAlert:))]
        fn ui_open_alert(&self, _timer: Option<&AnyObject>) {
            uitest::open_alert(self);
        }

        #[unsafe(method(restoreStatus:))]
        fn restore_status(&self, _timer: Option<&AnyObject>) {
            // A run that finished in the meantime has set its own status.
            if self.ivars().status.stringValue().to_string() == WAIT {
                let before = self.ivars().status_before_wait.borrow().clone();
                self.set_status(&before);
            }
        }
    }

    // SAFETY: NSObject already conforms to NSObjectProtocol, and every
    // NSApplicationDelegate and NSWindowDelegate method is optional; the ones
    // implemented below have the protocols' signatures.
    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSWindowDelegate for Controller {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            !self.refuse_quit()
        }
    }

    unsafe impl NSApplicationDelegate for Controller {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.launched();
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            // Under --ui-test the main window is never ordered in, so the
            // alert closing would count as the last window; the harness exits.
            self.ivars().ui.is_none()
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            if self.refuse_quit() {
                NSApplicationTerminateReply::TerminateCancel
            } else {
                NSApplicationTerminateReply::TerminateNow
            }
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker, ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSObject's designated initialiser takes no arguments.
        unsafe { msg_send![super(this), init] }
    }

    fn mtm(&self) -> MainThreadMarker {
        MainThreadMarker::from(self)
    }

    fn launched(&self) {
        let iv = self.ivars();
        iv.window.center();
        if iv.ui.is_some() {
            // A background test instance must not take keystrokes from the user.
            iv.window.makeKeyWindow();
        } else {
            iv.window.makeKeyAndOrderFront(None);
            NSApplication::sharedApplication(self.mtm()).activate();
        }
        match (&iv.config, &iv.ui) {
            (Ok(_), _) => {
                self.start(Job::Scan);
                if iv.ui.is_some() {
                    uitest::start(self);
                }
            }
            (Err(msg), Some(h)) => {
                h.event(&format!("FAIL: {msg}"));
                std::process::exit(2);
            }
            (Err(msg), None) => {
                self.set_status(msg);
                self.set_busy(false);
                let alert = NSAlert::new(self.mtm());
                alert.setAlertStyle(NSAlertStyle::Critical);
                alert.setMessageText(&NSString::from_str("Cannot run Office License Removal"));
                alert.setInformativeText(&NSString::from_str(msg));
                alert.runModal();
            }
        }
    }

    fn set_status(&self, s: &str) {
        self.ivars().status.setStringValue(&NSString::from_str(s));
        if let Some(h) = &self.ivars().ui {
            h.event(&format!("status: {s}"));
        }
    }

    fn show_log(&self) {
        let iv = self.ivars();
        let text = NSString::from_str(&iv.log.borrow());
        iv.log_view.setString(&text);
        iv.log_view
            .scrollRangeToVisible(NSRange::new(text.length(), 0));
    }

    fn set_busy(&self, busy: bool) {
        let iv = self.ivars();
        let enabled = !busy && iv.config.is_ok();
        // The branded buttons draw their own fill, so dim them explicitly.
        let alpha = if enabled { 1.0 } else { 0.45 };
        for b in [&iv.rescan, &iv.remove] {
            b.setEnabled(enabled);
            b.setAlphaValue(alpha);
        }
    }

    /// True while a CLI run is in progress: quitting then would orphan an
    /// elevated remove, so say so for 3 s and keep the app open.
    fn refuse_quit(&self) -> bool {
        let iv = self.ivars();
        if iv.running.borrow().is_none() {
            return false;
        }
        let current = iv.status.stringValue().to_string();
        if current != WAIT {
            *iv.status_before_wait.borrow_mut() = current;
            self.set_status(WAIT);
            // SAFETY: the target is this Controller, which implements
            // `restoreStatus:` with one object argument; no userInfo is passed.
            unsafe {
                NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                    3.0,
                    self,
                    sel!(restoreStatus:),
                    None,
                    false,
                );
            }
        }
        true
    }

    fn confirm_and_remove(&self) {
        let Ok(cfg) = &self.ivars().config else {
            return;
        };
        let mtm = self.mtm();
        let alert = NSAlert::new(mtm);
        alert.setAlertStyle(NSAlertStyle::Critical);
        alert.setMessageText(&NSString::from_str(
            "Remove all Office licenses from this Mac?",
        ));
        alert.setInformativeText(&NSString::from_str(&format!(
            "Word, Excel, PowerPoint, Outlook and OneNote will be force-quit. \
             The Microsoft 365 / Office 2021 / 2024 license files and the Office \
             sign-in keychain items for {} will be deleted.\n\n\
             This cannot be undone. You will have to sign in to Office again to reactivate it.",
            cfg.user
        )));
        let field = NSTextField::new(mtm);
        field.setFrame(rect(0.0, 0.0, 260.0, 24.0));
        field.setPlaceholderString(Some(&NSString::from_str("Type REMOVE to confirm")));
        alert.setAccessoryView(Some(&field));
        // The first button is the default (Return), so Cancel goes first.
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.addButtonWithTitle(&NSString::from_str("Remove"));
        alert.layout();
        alert.window().setInitialFirstResponder(Some(&field));
        *self.ivars().alert.borrow_mut() = Some(alert.clone());
        let pressed_remove = alert.runModal() == NSAlertSecondButtonReturn;
        *self.ivars().alert.borrow_mut() = None;
        if pressed_remove && field.stringValue().to_string() == "REMOVE" {
            match sha256_hex(&cfg.cli) {
                Ok(sha256) => self.start(Job::Remove(sha256)),
                Err(e) => self.set_status(&format!("Failed: {e}")),
            }
        } else {
            self.set_status(CANCELLED);
        }
    }

    fn start(&self, job: Job) {
        let iv = self.ivars();
        let Ok(cfg) = &iv.config else { return };
        if iv.running.borrow().is_some() {
            return;
        }
        match job {
            Job::Scan => {
                iv.log.borrow_mut().clear();
                self.set_status("Scanning…");
            }
            Job::ScanAfterRemove => iv.log.borrow_mut().push_str("\nScan after remove:\n"),
            Job::Remove(_) => {
                iv.log.borrow_mut().push_str("\nRemove:\n");
                self.set_status("Removing…");
            }
        }
        self.show_log();
        self.set_busy(true);

        let (tx, rx) = mpsc::channel();
        let cfg = cfg.clone();
        let worker_job = job.clone();
        std::thread::spawn(move || {
            let _ = tx.send(execute(&cfg, worker_job));
        });
        // SAFETY: the target is this Controller, which implements `poll:`
        // with one object argument; no userInfo is passed.
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.1,
                self,
                sel!(poll:),
                None,
                true,
            )
        };
        *iv.running.borrow_mut() = Some(Running { job, rx, timer });
    }

    /// Main-thread side of a CLI run: AppKit objects are touched only here.
    fn poll(&self) {
        let iv = self.ivars();
        let (job, outcome) = {
            let running = iv.running.borrow();
            let Some(r) = running.as_ref() else { return };
            match r.rx.try_recv() {
                Ok(o) => (r.job.clone(), o),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => (
                    r.job.clone(),
                    Outcome::NotStarted("worker thread ended without a result".into()),
                ),
            }
        };
        if let Some(r) = iv.running.borrow_mut().take() {
            r.timer.invalidate();
        }
        self.set_busy(false);
        self.finish(job, outcome);
    }

    fn finish(&self, job: Job, outcome: Outcome) {
        let iv = self.ivars();
        let mut status = None;
        let mut rescan = false;
        {
            let mut log = iv.log.borrow_mut();
            match (&job, outcome) {
                // Only the elevated remove has a password prompt.
                (_, Outcome::PasswordCancelled) => {
                    let msg = "Cancelled at the password prompt; nothing was changed.";
                    log.push_str(msg);
                    log.push('\n');
                    status = Some(msg.to_owned());
                }
                (Job::Remove(_), Outcome::Exited { code, output }) => {
                    log.push_str(&output);
                    if !output.ends_with('\n') {
                        log.push('\n');
                    }
                    log.push_str(&format!("Finished (exit {})\n", exit_text(code)));
                    status = Some(match code {
                        Some(0) => "Done".to_owned(),
                        _ => format!("Failed (exit {})", exit_text(code)),
                    });
                    rescan = true;
                }
                (Job::Scan | Job::ScanAfterRemove, Outcome::Exited { code, output }) => {
                    log.push_str(&output);
                    if code != Some(0) {
                        log.push_str(&format!("Scan failed (exit {})\n", exit_text(code)));
                    }
                    if matches!(job, Job::Scan) {
                        status = Some(match code {
                            Some(0) => "Scan complete".to_owned(),
                            _ => format!("Scan failed (exit {})", exit_text(code)),
                        });
                    }
                }
                (_, Outcome::NotStarted(msg)) => {
                    log.push_str(&format!("Could not run: {msg}\n"));
                    status = Some(format!("Failed: {msg}"));
                }
            }
        }
        self.show_log();
        if let Some(s) = status {
            self.set_status(&s);
        }
        if rescan {
            self.start(Job::ScanAfterRemove);
        }
    }
}

fn button(
    title: &str,
    action: objc2::runtime::Sel,
    frame: NSRect,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    // SAFETY: `action` is a selector the target (set later, or nil for the
    // responder chain) implements with the standard action signature.
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, Some(action), mtm)
    };
    b.setFrame(frame);
    b
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn srgb(r: u8, g: u8, b: u8) -> Retained<NSColor> {
    let c = |v: u8| f64::from(v) / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(c(r), c(g), c(b), 1.0)
}

/// Flat filled button. `bezelColor` is only painted while the window is key in
/// the active app, so the fill comes from the button's own layer instead.
fn paint(b: &NSButton, fill: &NSColor) {
    b.setBordered(false);
    b.setWantsLayer(true);
    if let Some(layer) = b.layer() {
        layer.setBackgroundColor(Some(&fill.CGColor()));
        layer.setCornerRadius(6.0);
        layer.setMasksToBounds(true);
    }
}

/// Centred button title in one colour and font.
fn styled(title: &str, color: &NSColor, font: &NSFont) -> Retained<NSAttributedString> {
    let para = NSMutableParagraphStyle::new();
    para.setAlignment(NSTextAlignment::Center);
    // SAFETY: reading immutable AppKit string constants.
    let keys = unsafe {
        [
            NSForegroundColorAttributeName,
            NSFontAttributeName,
            NSParagraphStyleAttributeName,
        ]
    };
    let values: [&AnyObject; 3] = [color.as_ref(), font.as_ref(), para.as_ref()];
    let attrs = NSDictionary::from_slices(&keys, &values);
    // SAFETY: each value has the class its key requires (NSColor, NSFont,
    // NSParagraphStyle).
    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(title),
            Some(&attrs),
        )
    }
}

fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let bar = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    // SAFETY: `terminate:` is NSApplication's standard action; the nil-targeted
    // item reaches NSApp through the responder chain.
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&format!("Quit {TITLE}")),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    app_menu.addItem(&quit);
    app_item.setSubmenu(Some(&app_menu));
    bar.addItem(&app_item);
    bar
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ui = match uitest::parse_args(&args) {
        Ok(None) => None,
        Ok(Some((dir, flow))) => match uitest::Harness::new(dir, flow) {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("error: --ui-test: {e}");
                std::process::exit(64);
            }
        },
        Err(e) => {
            eprintln!("error: {e}\nusage: OfficeLicenseRemoval [--ui-test <dir> [--flow scan|cancel|remove]]");
            std::process::exit(64);
        }
    };

    let mtm = MainThreadMarker::new().expect("AppKit must start on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.setMainMenu(Some(&main_menu(mtm)));

    let config = Config::load();
    let lab = matches!(&config, Ok(c) if c.lab.is_some());
    if let Some(h) = &ui {
        if h.flow != uitest::Flow::Scan && !lab {
            h.event(&format!(
                "refused: {} flow requires lab mode",
                h.flow.name()
            ));
            std::process::exit(3);
        }
    }

    let crimson = srgb(0xC0, 0x00, 0x27);
    let gold = srgb(0xF4, 0xBD, 0x47);
    let black = srgb(0x00, 0x00, 0x00);
    let white = srgb(0xFF, 0xFF, 0xFF);

    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;
    // SAFETY: a freshly allocated window initialised once. Retained owns it,
    // so AppKit must not also release it on close (objc2's documented rule).
    let window = unsafe {
        let w = NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect(0.0, 0.0, W, H),
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        w.setReleasedWhenClosed(false);
        w
    };
    let title = if lab {
        format!("{TITLE} — LAB MODE")
    } else {
        TITLE.to_owned()
    };
    // The title stays set (Window menu, accessibility, harness) but is drawn
    // by the gold label in the header band instead.
    window.setTitle(&NSString::from_str(&title));
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    window.setTitlebarAppearsTransparent(true);
    // SAFETY: reading an immutable AppKit string constant.
    let dark = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua });
    window.setAppearance(dark.as_deref());
    window.setBackgroundColor(Some(&black));
    window.setContentMinSize(NSSize::new(520.0, 320.0));
    let content = window
        .contentView()
        .expect("titled window has a content view");

    // Crimson header band behind the traffic lights, full width, 80 pt tall.
    let band = NSBox::new(mtm);
    band.setBoxType(NSBoxType::Custom);
    band.setTitlePosition(NSTitlePosition::NoTitle);
    band.setBorderWidth(0.0);
    band.setFillColor(&crimson);
    band.setFrame(rect(0.0, H - BAND, W, BAND));
    band.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
    );

    let heading = NSTextField::labelWithString(&NSString::from_str(&title), mtm);
    heading.setFont(Some(&NSFont::boldSystemFontOfSize(20.0)));
    heading.setTextColor(Some(&gold));
    // Lower part of the band, below the traffic lights.
    heading.setFrame(rect(20.0, H - BAND + 26.0, W - 40.0, 26.0));
    heading.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
    );

    let status = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    status.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    status.setTextColor(Some(&white));
    status.setAlignment(NSTextAlignment::Right);
    status.setFrame(rect(20.0, H - BAND + 6.0, W - 40.0, 18.0));
    status.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
    );

    let scroll = NSTextView::scrollableTextView(mtm);
    scroll.setFrame(rect(20.0, 60.0, W - 40.0, H - BAND - 70.0));
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    scroll.setBorderType(NSBorderType::NoBorder);
    scroll.setBackgroundColor(&black);
    let log_view = scroll
        .documentView()
        .expect("scrollableTextView has a document view")
        .downcast::<NSTextView>()
        .expect("scrollableTextView's document view is an NSTextView");
    log_view.setEditable(false);
    log_view.setRichText(false);
    // Weight 0.0 is NSFontWeightRegular.
    log_view.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(12.0, 0.0)));
    log_view.setTextColor(Some(&white));
    log_view.setBackgroundColor(&black);

    let rescan = button("Rescan", sel!(rescan:), rect(20.0, 16.0, 110.0, 32.0), mtm);
    paint(&rescan, &gold);
    rescan.setAttributedTitle(&styled(
        "Rescan",
        &black,
        &NSFont::boldSystemFontOfSize(13.0),
    ));
    let remove = button(
        "Remove Licenses…",
        sel!(removeLicenses:),
        rect(136.0, 16.0, 170.0, 32.0),
        mtm,
    );
    paint(&remove, &crimson);
    remove.setAttributedTitle(&styled(
        "Remove Licenses…",
        &white,
        &NSFont::boldSystemFontOfSize(13.0),
    ));
    let quit = button(
        "Quit",
        sel!(terminate:),
        rect(W - 120.0, 16.0, 100.0, 32.0),
        mtm,
    );
    quit.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);

    let views: [&NSView; 7] = [&band, &heading, &status, &scroll, &rescan, &remove, &quit];
    for v in views {
        content.addSubview(v);
    }

    let controller = Controller::new(
        mtm,
        Ivars {
            config,
            window,
            status,
            log_view,
            rescan: rescan.clone(),
            remove: remove.clone(),
            log: RefCell::new(String::new()),
            running: RefCell::new(None),
            status_before_wait: RefCell::new(String::new()),
            alert: RefCell::new(None),
            ui,
        },
    );
    for b in [&rescan, &remove] {
        // SAFETY: Controller implements both buttons' actions (rescan:,
        // removeLicenses:) and outlives them (it lives until app.run returns).
        unsafe { b.setTarget(Some(&controller)) };
    }
    app.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    controller
        .ivars()
        .window
        .setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_matches_quoted_form_of() {
        assert_eq!(sh_quote("/a b/c"), "'/a b/c'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn elevated_script_quotes_path_user_and_hash() {
        let s = elevated_script(
            "/Apps/Bob's \"Tools\"/Office License Removal.app/Contents/MacOS/office-license-removal",
            "bob",
            &"0123456789abcdef".repeat(4),
        );
        assert_eq!(
            s,
            r#"do shell script "( t=$(/usr/bin/mktemp -d /private/tmp/olr.XXXXXX) || exit 1; /bin/cp '/Apps/Bob'\\''s \"Tools\"/Office License Removal.app/Contents/MacOS/office-license-removal' \"$t/office-license-removal\" || exit 1; h=$(/usr/bin/shasum -a 256 \"$t/office-license-removal\"); [ \"${h%% *}\" = '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef' ] || { echo 'REFUSED: office-license-removal changed since confirmation'; exit 3; }; exec \"$t/office-license-removal\" remove --yes --user 'bob' ) 2>&1; echo OLR_EXIT=$?" with administrator privileges without altering line endings"#
        );
    }

    #[test]
    fn sha256_hex_reads_shasum_and_fails_closed() {
        assert_eq!(
            sha256_hex("/dev/null").unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(sha256_hex("/nonexistent/office-license-removal").is_err());
    }

    #[test]
    fn exit_marker_is_split_off() {
        assert_eq!(
            split_exit_marker("line 1\nline 2\nOLR_EXIT=2\n"),
            ("line 1\nline 2\n".to_owned(), Some(2))
        );
        assert_eq!(
            split_exit_marker("no marker\n"),
            ("no marker\n".to_owned(), None)
        );
    }

    #[test]
    fn combined_output_keeps_order_and_exit_code() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo out1; echo err1 >&2; echo out2; exit 3"]);
        let (code, output) = run_combined(cmd).unwrap();
        assert_eq!(code, Some(3));
        assert_eq!(output, "out1\nerr1\nout2\n");
    }

    #[test]
    fn lab_args_add_overrides() {
        let mut c = Config {
            cli: "/x/office-license-removal".into(),
            user: "bob".into(),
            lab: None,
        };
        assert_eq!(c.args("scan"), ["scan", "--user", "bob"]);
        c.lab = Some(("/f/home".into(), "/f/root".into()));
        assert_eq!(
            c.args("remove"),
            [
                "remove",
                "--user",
                "bob",
                "--home",
                "/f/home",
                "--system-root",
                "/f/root"
            ]
        );
    }
}
