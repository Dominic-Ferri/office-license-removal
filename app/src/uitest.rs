//! `--ui-test <dir> [--flow scan|cancel|remove]`: drives the window through
//! the same actions a user triggers and saves PNG captures and `events.log`
//! to `<dir>`. Captures render the views in-process, so no Screen Recording
//! permission is needed.

use std::cell::{Cell, RefCell};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use objc2::{sel, DefinedClass};
use objc2_app_kit::{
    NSApplication, NSBitmapImageFileType, NSEventModifierFlags, NSMenu, NSModalPanelRunLoopMode,
    NSTextField, NSWindow,
};
use objc2_foundation::{MainThreadMarker, NSDefaultRunLoopMode, NSDictionary, NSRunLoop, NSTimer};

use crate::{Controller, CANCELLED};

/// A run that has not finished by then is reported as FAIL.
const DEADLINE: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, PartialEq)]
pub enum Flow {
    Scan,
    Cancel,
    Remove,
}

impl Flow {
    pub fn name(self) -> &'static str {
        match self {
            Flow::Scan => "scan",
            Flow::Cancel => "cancel",
            Flow::Remove => "remove",
        }
    }
}

/// `Ok(None)` when `--ui-test` is absent; other arguments are then ignored.
pub fn parse_args(args: &[String]) -> Result<Option<(PathBuf, Flow)>, String> {
    if !args.iter().any(|a| a == "--ui-test") {
        return Ok(None);
    }
    let mut dir = None;
    let mut flow = Flow::Scan;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--ui-test" => {
                dir = Some(PathBuf::from(
                    it.next().ok_or("--ui-test needs a directory")?,
                ))
            }
            "--flow" => {
                flow = match it.next().map(String::as_str) {
                    Some("scan") => Flow::Scan,
                    Some("cancel") => Flow::Cancel,
                    Some("remove") => Flow::Remove,
                    other => {
                        return Err(format!(
                            "--flow must be scan, cancel or remove, not {other:?}"
                        ))
                    }
                }
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }
    Ok(dir.map(|d| (d, flow)))
}

#[derive(Clone, Copy)]
enum Step {
    WaitScan,
    AlertOpening,
    AlertShown,
    AlertFilled,
    AfterClick,
    Exiting(Instant, i32),
}

pub struct Harness {
    pub flow: Flow,
    dir: PathBuf,
    t0: Instant,
    log: RefCell<File>,
    step: Cell<Step>,
    capture_failed: Cell<bool>,
    saw_removing: Cell<bool>,
}

impl Harness {
    pub fn new(dir: PathBuf, flow: Flow) -> std::io::Result<Harness> {
        fs::create_dir_all(&dir)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.log"))?;
        Ok(Harness {
            flow,
            dir,
            t0: Instant::now(),
            log: RefCell::new(log),
            step: Cell::new(Step::WaitScan),
            capture_failed: Cell::new(false),
            saw_removing: Cell::new(false),
        })
    }

    pub fn event(&self, msg: &str) {
        let ms = self.t0.elapsed().as_millis();
        let _ = writeln!(self.log.borrow_mut(), "{ms} {msg}");
    }

    fn capture(&self, window: &NSWindow, name: &str) {
        let file = format!("{name}.png");
        let res = png(window)
            .ok_or_else(|| "no image".to_owned())
            .and_then(|bytes| fs::write(self.dir.join(&file), bytes).map_err(|e| e.to_string()));
        match res {
            Ok(()) => self.event(&format!("capture: {file}")),
            Err(e) => {
                self.capture_failed.set(true);
                self.event(&format!("capture failed: {file}: {e}"));
            }
        }
    }

    fn write_menu(&self, mtm: MainThreadMarker) {
        let mut out = String::new();
        if let Some(bar) = NSApplication::sharedApplication(mtm).mainMenu() {
            menu_lines(&bar, 0, &mut out);
        }
        match fs::write(self.dir.join("menu.txt"), out) {
            Ok(()) => self.event("wrote: menu.txt"),
            Err(e) => self.event(&format!("menu.txt failed: {e}")),
        }
    }

    /// Log the verdict and exit `delay` later from the tick timer.
    fn finish(&self, failures: &[String], delay: Duration) {
        let mut failures = failures.to_vec();
        if self.capture_failed.get() {
            failures.push("a capture failed".into());
        }
        let code = if failures.is_empty() {
            self.event(&format!("PASS: {} flow", self.flow.name()));
            0
        } else {
            for f in &failures {
                self.event(&format!("FAIL: {f}"));
            }
            2
        };
        self.event(&format!("exit {code}"));
        self.step.set(Step::Exiting(Instant::now() + delay, code));
    }
}

/// PNG of the window's frame view (content plus title bar).
fn png(window: &NSWindow) -> Option<Vec<u8>> {
    let content = window.contentView()?;
    // SAFETY: the result is a Retained (+1) reference, so the frame view
    // stays alive while it is used here.
    let frame = unsafe { content.superview() }?;
    let rect = frame.bounds();
    let rep = frame.bitmapImageRepForCachingDisplayInRect(rect)?;
    frame.cacheDisplayInRect_toBitmapImageRep(rect, &rep);
    // SAFETY: an empty properties dictionary has the declared key and value types.
    let data = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(data.to_vec())
}

fn menu_lines(menu: &NSMenu, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    out.push_str(&format!("{pad}menu \"{}\"\n", menu.title()));
    for item in menu.itemArray().iter() {
        let mods = item.keyEquivalentModifierMask();
        let cmd = if mods.contains(NSEventModifierFlags::Command) {
            " (cmd)"
        } else {
            ""
        };
        out.push_str(&format!(
            "{pad}  item \"{}\" key \"{}\" modifiers 0x{:x}{cmd}\n",
            item.title(),
            item.keyEquivalent(),
            mods.0
        ));
        if let Some(sub) = item.submenu() {
            menu_lines(&sub, depth + 2, out);
        }
    }
}

/// Start the harness timer. It runs in the modal panel mode as well, so it
/// keeps firing while `NSAlert::runModal` is on the stack.
pub fn start(c: &Controller) {
    // SAFETY: the target is the Controller, which implements `uiTick:` with
    // one object argument; no userInfo is passed.
    let timer = unsafe {
        NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
            0.1,
            c,
            sel!(uiTick:),
            None,
            true,
        )
    };
    let run_loop = NSRunLoop::currentRunLoop();
    // SAFETY: both modes are immutable framework constants, and the timer is
    // a valid, not yet scheduled NSTimer.
    unsafe {
        run_loop.addTimer_forMode(&timer, NSDefaultRunLoopMode);
        run_loop.addTimer_forMode(&timer, NSModalPanelRunLoopMode);
    }
}

/// Press the Remove button from a one-shot default-mode timer. The tick
/// timer cannot do it itself: a timer does not fire again while its own
/// callback is blocked in `runModal`.
pub fn open_alert(c: &Controller) {
    // SAFETY: a nil sender is valid for an action.
    unsafe { c.ivars().remove.performClick(None) };
}

pub fn tick(c: &Controller) {
    let iv = c.ivars();
    let Some(h) = &iv.ui else { return };
    let mtm = MainThreadMarker::from(c);
    let step = h.step.get();
    if !matches!(step, Step::Exiting(..)) && h.t0.elapsed() > DEADLINE {
        h.finish(
            &[format!("timed out after {}s", DEADLINE.as_secs())],
            Duration::ZERO,
        );
        return;
    }
    let status = iv.status.stringValue().to_string();
    let modal = NSApplication::sharedApplication(mtm).modalWindow();
    let alert = iv.alert.borrow().clone();
    match step {
        Step::WaitScan => {
            if iv.running.borrow().is_some() {
                return;
            }
            h.capture(&iv.window, "01-scan");
            h.write_menu(mtm);
            if h.flow == Flow::Scan {
                let mut failures = Vec::new();
                if status != "Scan complete" {
                    failures.push(format!("status is '{status}', expected 'Scan complete'"));
                }
                h.finish(&failures, Duration::from_secs(1));
                return;
            }
            // SAFETY: the target is the Controller, which implements
            // `uiOpenAlert:` with one object argument; no userInfo is passed.
            unsafe {
                NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                    0.0,
                    c,
                    sel!(uiOpenAlert:),
                    None,
                    false,
                );
            }
            h.step.set(Step::AlertOpening);
        }
        Step::AlertOpening => {
            let (Some(win), Some(_)) = (modal, alert) else {
                return;
            };
            h.capture(&win, "02-alert");
            h.step.set(Step::AlertShown);
        }
        Step::AlertShown => {
            let Some(alert) = alert else { return };
            let text = if h.flow == Flow::Remove {
                "REMOVE"
            } else {
                "nope"
            };
            match alert
                .accessoryView()
                .and_then(|v| v.downcast::<NSTextField>().ok())
            {
                Some(field) => {
                    field.setStringValue(&objc2_foundation::NSString::from_str(text));
                    h.event(&format!("typed: {text}"));
                }
                None => h.event("alert has no text field"),
            }
            h.step.set(Step::AlertFilled);
        }
        Step::AlertFilled => {
            let (Some(win), Some(alert)) = (modal, alert) else {
                return;
            };
            h.capture(&win, "03-alert-filled");
            let buttons = alert.buttons();
            if buttons.count() < 2 {
                h.finish(&["alert has fewer than 2 buttons".into()], Duration::ZERO);
                return;
            }
            let second = buttons.objectAtIndex(1);
            h.event(&format!("click: {}", second.title()));
            h.step.set(Step::AfterClick);
            // SAFETY: a nil sender is valid for an action.
            unsafe { second.performClick(None) };
        }
        Step::AfterClick => {
            if alert.is_some() {
                return;
            }
            let log_text = iv.log_view.string().to_string();
            match h.flow {
                Flow::Cancel => {
                    if iv.running.borrow().is_some() {
                        return;
                    }
                    h.capture(&iv.window, "04-after-cancel");
                    let mut failures = Vec::new();
                    if status != CANCELLED {
                        failures.push(format!("status is '{status}', expected '{CANCELLED}'"));
                    }
                    if log_text.contains("Starting Office for Mac License Removal") {
                        failures.push("remove ran".into());
                    }
                    h.finish(&failures, Duration::ZERO);
                }
                Flow::Remove => {
                    if status == "Removing…" && !h.saw_removing.get() {
                        h.saw_removing.set(true);
                        h.capture(&iv.window, "04-removing");
                    }
                    if iv.running.borrow().is_some() || !log_text.contains("\nScan after remove:\n")
                    {
                        return;
                    }
                    h.capture(&iv.window, "05-after-remove");
                    let mut failures = Vec::new();
                    if status != "Done" {
                        failures.push(format!("status is '{status}', expected 'Done'"));
                    }
                    for want in [
                        "Removed all Office flighting data",
                        "Completed Office for Mac License Removal",
                    ] {
                        if !log_text.contains(want) {
                            failures.push(format!("log view lacks '{want}'"));
                        }
                    }
                    h.finish(&failures, Duration::ZERO);
                }
                Flow::Scan => {}
            }
        }
        Step::Exiting(at, code) => {
            if Instant::now() >= at {
                std::process::exit(code);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_ui_test_flags() {
        assert!(parse_args(&[]).unwrap().is_none());
        let (d, f) = parse_args(&args(&["--ui-test", "/t"])).unwrap().unwrap();
        assert!(d == std::path::Path::new("/t") && f == Flow::Scan);
        let (_, f) = parse_args(&args(&["--ui-test", "/t", "--flow", "remove"]))
            .unwrap()
            .unwrap();
        assert!(f == Flow::Remove);
        assert!(parse_args(&args(&["--ui-test"])).is_err());
        assert!(parse_args(&args(&["--ui-test", "/t", "--flow", "x"])).is_err());
        assert!(parse_args(&args(&["--ui-test", "/t", "--bogus"])).is_err());
    }
}
