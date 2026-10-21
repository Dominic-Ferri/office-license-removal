//! Terminal UI (`office-license-removal tui`). A ratatui front end that runs
//! this binary's own `scan` and `remove` commands as child processes, so the
//! UI needs nothing beyond the CLI file itself.

mod smoke;

use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::{Frame, Terminal};

use crate::sys;

const TITLE: &str = "Office License Removal";
const CANCELLED: &str = "Cancelled; nothing was changed.";
const PASSWORD_CANCELLED: &str = "Cancelled at the password prompt; nothing was changed.";
const MIN_W: u16 = 50;
const MIN_H: u16 = 16;
/// First line the root script prints. Until it arrives, no root code ran.
const ROOT_STARTED: &str = "OLR_ROOT_STARTED";
/// Runs as root with $1 = CLI path, $2 = user, $3 = SHA-256 taken at
/// confirmation. Root must not run the CLI from a folder the user can write
/// to, so it copies the CLI into a new root-only temp dir (left in place),
/// checks the copy's hash and runs the copy.
const SUDO_REMOVE: &str = r#"print -r -- OLR_ROOT_STARTED
t=$(/usr/bin/mktemp -d /private/tmp/olr.XXXXXX) || exit 1
/bin/cp "$1" "$t/office-license-removal" || exit 1
h=$(/usr/bin/shasum -a 256 "$t/office-license-removal")
[[ ${h[1,64]} == "$3" ]] || { print -r -- 'REFUSED: office-license-removal changed since confirmation'; exit 3; }
exec "$t/office-license-removal" remove --yes --user "$2"
"#;

#[derive(Clone, Copy)]
struct Palette {
    crimson: Color,
    gold: Color,
    black: Color,
    white: Color,
    gray: Color,
}

impl Palette {
    const TRUE_COLOR: Palette = Palette {
        crimson: Color::Rgb(0xC0, 0x00, 0x27),
        gold: Color::Rgb(0xF4, 0xBD, 0x47),
        black: Color::Rgb(0, 0, 0),
        white: Color::Rgb(0xFF, 0xFF, 0xFF),
        gray: Color::Rgb(0x6C, 0x6C, 0x6C),
    };
    /// Nearest xterm-256 colours, for terminals without 24-bit colour.
    const INDEXED: Palette = Palette {
        crimson: Color::Indexed(124),
        gold: Color::Indexed(221),
        black: Color::Indexed(16),
        white: Color::Indexed(231),
        gray: Color::Indexed(242),
    };

    fn detect() -> Palette {
        match std::env::var("COLORTERM").as_deref() {
            Ok("truecolor" | "24bit") => Palette::TRUE_COLOR,
            _ => Palette::INDEXED,
        }
    }
}

struct Scope {
    /// This binary; the UI runs it for `scan` and `remove`.
    cli: PathBuf,
    user: String,
    /// Lab mode: (--home, --system-root) of a fixture tree.
    lab: Option<(PathBuf, PathBuf)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Focus {
    #[default]
    Input,
    Cancel,
    Remove,
}

impl Focus {
    fn next(self) -> Focus {
        match self {
            Focus::Input => Focus::Cancel,
            Focus::Cancel => Focus::Remove,
            Focus::Remove => Focus::Input,
        }
    }

    fn prev(self) -> Focus {
        self.next().next()
    }
}

#[derive(Default)]
struct Confirm {
    input: String,
    focus: Focus,
}

#[derive(Clone, Copy)]
enum Job {
    Scan { after_remove: bool },
    LabRemove,
}

struct Done {
    job: Job,
    code: Option<i32>,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Hit {
    Rescan,
    Remove,
    Quit,
    Input,
    Cancel,
    ConfirmRemove,
}

/// What the event loop must do after an input event.
#[derive(Debug, PartialEq)]
enum Action {
    None,
    /// The user confirmed a real remove: suspend the UI and run sudo.
    SudoRemove,
}

struct App {
    scope: Scope,
    pal: Palette,
    /// Output rows as (text, is_note); notes are bold gold.
    log: Vec<(String, bool)>,
    scroll: usize,
    max_scroll: usize,
    page: usize,
    follow: bool,
    status: String,
    busy: Option<Receiver<Done>>,
    confirm: Option<Confirm>,
    /// Clickable areas from the last draw.
    hits: Vec<(Rect, Hit)>,
    quit: bool,
}

impl App {
    fn new(scope: Scope, pal: Palette) -> App {
        App {
            scope,
            pal,
            log: Vec::new(),
            scroll: 0,
            max_scroll: 0,
            page: 1,
            follow: true,
            status: String::new(),
            busy: None,
            confirm: None,
            hits: Vec::new(),
            quit: false,
        }
    }

    // ------------------------------------------------------------ output

    fn push(&mut self, text: &str) {
        for line in text.lines() {
            self.log.push((sanitize(line), false));
        }
    }

    fn note(&mut self, text: &str) {
        self.log.push((sanitize(text), true));
    }

    fn blank(&mut self) {
        self.log.push((String::new(), false));
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.max_scroll);
        self.follow = self.scroll >= self.max_scroll;
    }

    // ------------------------------------------------------------ jobs

    /// Run `scan` or a lab-mode `remove` on a worker thread.
    fn start(&mut self, job: Job) {
        let sub = match job {
            Job::Scan { after_remove } => {
                if !after_remove {
                    self.log.clear();
                    self.follow = true;
                    self.status = "Scanning…".into();
                }
                "scan"
            }
            Job::LabRemove => {
                self.blank();
                self.note("Remove:");
                self.status = "Removing…".into();
                "remove"
            }
        };
        let mut cmd = Command::new(&self.scope.cli);
        cmd.arg(sub);
        if matches!(job, Job::LabRemove) {
            cmd.arg("--yes");
        }
        cmd.arg("--user").arg(&self.scope.user);
        if let Some((home, root)) = &self.scope.lab {
            cmd.arg("--home").arg(home).arg("--system-root").arg(root);
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (code, text) = run_merged(cmd);
            let _ = tx.send(Done { job, code, text });
        });
        self.busy = Some(rx);
    }

    /// Apply a finished job, waiting up to `wait` for one.
    fn poll(&mut self, wait: Duration) {
        let got = match &self.busy {
            Some(rx) => rx.recv_timeout(wait),
            None => return,
        };
        match got {
            Ok(done) => {
                self.busy = None;
                self.finish(done);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                self.busy = None;
                self.status = "Failed (the worker thread stopped)".into();
            }
        }
    }

    fn finish(&mut self, done: Done) {
        self.push(&done.text);
        match done.job {
            Job::Scan { after_remove } => {
                let failed = format!("Scan failed ({})", exit_text(done.code));
                if done.code != Some(0) {
                    self.note(&failed);
                }
                // Like the native app, the rescan after a remove keeps the
                // remove's status.
                if !after_remove {
                    self.status = if done.code == Some(0) {
                        "Scan complete".into()
                    } else {
                        failed
                    };
                }
            }
            Job::LabRemove => self.removed(done.code),
        }
    }

    /// Report a finished remove, then rescan.
    fn removed(&mut self, code: Option<i32>) {
        self.note(&format!("Finished ({})", exit_text(code)));
        self.status = if code == Some(0) {
            "Done".into()
        } else {
            format!("Failed ({})", exit_text(code))
        };
        self.blank();
        self.note("Scan after remove:");
        self.start(Job::Scan { after_remove: true });
    }

    fn after_sudo(&mut self, result: io::Result<PrivRun>) {
        self.blank();
        self.note("Remove (sudo):");
        let run = match result {
            Ok(run) => run,
            Err(e) => {
                let msg = format!("Could not run sudo: {e}. Nothing was changed.");
                self.note(&msg);
                self.status = msg;
                return;
            }
        };
        for line in &run.lines {
            self.log.push((sanitize(line), false));
        }
        match outcome(&run) {
            Outcome::NotStarted(msg) => {
                self.note(&msg);
                self.status = msg;
            }
            Outcome::Finished { code, interrupted } => {
                if interrupted {
                    self.note("Interrupted during the remove. The scan below shows what changed.");
                }
                self.removed(code);
            }
        }
    }

    // ------------------------------------------------------------ input

    fn rescan(&mut self) {
        if self.busy.is_none() && self.confirm.is_none() {
            self.start(Job::Scan {
                after_remove: false,
            });
        }
    }

    fn open_confirm(&mut self) {
        if self.busy.is_none() {
            self.confirm = Some(Confirm::default());
        }
    }

    fn cancel_confirm(&mut self) -> Action {
        self.confirm = None;
        self.status = CANCELLED.into();
        Action::None
    }

    /// The dialog's Remove button: only the exact text REMOVE goes ahead.
    fn press_remove(&mut self) -> Action {
        let confirmed = self.confirm.take().is_some_and(|c| c.input == "REMOVE");
        if !confirmed {
            self.status = CANCELLED.into();
            Action::None
        } else if self.scope.lab.is_some() {
            self.start(Job::LabRemove);
            Action::None
        } else {
            Action::SudoRemove
        }
    }

    fn on_key(&mut self, k: KeyEvent) -> Action {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return Action::None;
        }
        if self.confirm.is_some() {
            return self.confirm_key(k);
        }
        let page = self.page.max(2) as isize - 1;
        match k.code {
            KeyCode::Char('r' | 'R') => self.rescan(),
            KeyCode::Char('x' | 'X') => self.open_confirm(),
            KeyCode::Char('q' | 'Q') => self.quit = true,
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::PageDown => self.scroll_by(page),
            KeyCode::Home => self.scroll_by(isize::MIN),
            KeyCode::End => self.scroll_by(isize::MAX),
            _ => {}
        }
        Action::None
    }

    fn confirm_key(&mut self, k: KeyEvent) -> Action {
        let Some(c) = self.confirm.as_mut() else {
            return Action::None;
        };
        let typing = c.focus == Focus::Input;
        match k.code {
            KeyCode::Esc => return self.cancel_confirm(),
            KeyCode::Tab => c.focus = c.focus.next(),
            KeyCode::BackTab => c.focus = c.focus.prev(),
            KeyCode::Left if !typing => c.focus = Focus::Cancel,
            KeyCode::Right if !typing => c.focus = Focus::Remove,
            // Return in the field means Cancel, as Cancel is the default button.
            KeyCode::Enter | KeyCode::Char(' ') if !typing => {
                return match c.focus {
                    Focus::Remove => self.press_remove(),
                    _ => self.cancel_confirm(),
                };
            }
            KeyCode::Enter => return self.cancel_confirm(),
            KeyCode::Backspace if typing => {
                c.input.pop();
            }
            KeyCode::Char(ch)
                if typing
                    && !k.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    )
                    && c.input.chars().count() < 32 =>
            {
                c.input.push(ch)
            }
            _ => {}
        }
        Action::None
    }

    fn on_mouse(&mut self, m: MouseEvent) -> Action {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let at = Position::new(m.column, m.row);
                let Some(hit) = self.hits.iter().find(|(r, _)| r.contains(at)).map(|h| h.1) else {
                    return Action::None;
                };
                match hit {
                    Hit::Rescan => self.rescan(),
                    Hit::Remove => self.open_confirm(),
                    Hit::Quit => self.quit = true,
                    Hit::Input => {
                        if let Some(c) = self.confirm.as_mut() {
                            c.focus = Focus::Input;
                        }
                    }
                    Hit::Cancel => return self.cancel_confirm(),
                    Hit::ConfirmRemove => return self.press_remove(),
                }
            }
            MouseEventKind::ScrollUp if self.confirm.is_none() => self.scroll_by(-3),
            MouseEventKind::ScrollDown if self.confirm.is_none() => self.scroll_by(3),
            _ => {}
        }
        Action::None
    }

    // ------------------------------------------------------------ drawing

    fn draw(&mut self, f: &mut Frame) {
        let p = self.pal;
        let area = f.area();
        self.hits.clear();
        f.render_widget(
            Block::new().style(Style::new().fg(p.white).bg(p.black)),
            area,
        );
        if area.width < MIN_W || area.height < MIN_H {
            let msg = format!("Make this window larger (at least {MIN_W} × {MIN_H}).");
            f.render_widget(
                Paragraph::new(msg)
                    .style(Style::new().fg(p.gold))
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
        let [title, _, log, status, buttons, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_title(f, title);
        self.draw_log(f, log.inner(Margin::new(1, 0)));
        f.render_widget(
            Paragraph::new(self.status.as_str())
                .style(Style::new().fg(p.gold).add_modifier(Modifier::BOLD)),
            status.inner(Margin::new(2, 0)),
        );
        self.draw_buttons(f, buttons);
        self.draw_footer(f, footer);
        if self.confirm.is_some() {
            self.draw_confirm(f, area);
        }
    }

    fn draw_title(&self, f: &mut Frame, area: Rect) {
        let p = self.pal;
        let label = match self.scope.lab {
            Some(_) => format!("{TITLE} · LAB MODE"),
            None => TITLE.to_owned(),
        };
        f.render_widget(
            Paragraph::new(vec![Line::default(), Line::from(label)])
                .style(
                    Style::new()
                        .fg(p.gold)
                        .bg(p.crimson)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center),
            area,
        );
    }

    fn draw_log(&mut self, f: &mut Frame, area: Rect) {
        let p = self.pal;
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(p.crimson))
            .title(Line::styled(
                " Output ",
                Style::new().fg(p.gold).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        f.render_widget(block, area);
        // The last column holds the scrollbar.
        let text_area = Rect {
            width: inner.width.saturating_sub(1),
            ..inner
        };
        let plain = Style::new().fg(p.white);
        let note = Style::new().fg(p.gold).add_modifier(Modifier::BOLD);
        let rows: Vec<Line> = self
            .log
            .iter()
            .flat_map(|(text, is_note)| {
                let style = if *is_note { note } else { plain };
                wrap(text, text_area.width as usize)
                    .into_iter()
                    .map(move |row| Line::styled(row, style))
            })
            .collect();
        let height = inner.height as usize;
        self.page = height;
        self.max_scroll = rows.len().saturating_sub(height);
        self.scroll = if self.follow {
            self.max_scroll
        } else {
            self.scroll.min(self.max_scroll)
        };
        let visible: Vec<Line> = rows.into_iter().skip(self.scroll).take(height).collect();
        f.render_widget(Paragraph::new(visible), text_area);
        if self.max_scroll > 0 {
            let mut state = ScrollbarState::new(self.max_scroll + 1)
                .viewport_content_length(height)
                .position(self.scroll);
            let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_style(Style::new().fg(p.crimson))
                .track_style(Style::new().fg(p.black));
            f.render_stateful_widget(bar, inner, &mut state);
        }
    }

    /// A 3-row button. `filled` paints the whole button in `accent`.
    #[allow(clippy::too_many_arguments)]
    fn button(
        &self,
        f: &mut Frame,
        area: Rect,
        label: &str,
        accent: Color,
        filled: bool,
        enabled: bool,
        focused: bool,
    ) {
        let p = self.pal;
        let (fg, bg, edge) = match (enabled, filled) {
            (false, _) => (p.gray, p.black, p.gray),
            (true, true) => (p.white, accent, accent),
            (true, false) => (accent, p.black, accent),
        };
        let edge = if focused { p.gold } else { edge };
        let mut text = Style::new().fg(fg).bg(bg);
        if enabled {
            text = text.add_modifier(Modifier::BOLD);
        }
        let block = Block::bordered()
            .border_type(if focused {
                BorderType::Thick
            } else {
                BorderType::Rounded
            })
            .border_style(Style::new().fg(edge).bg(bg))
            .style(Style::new().bg(bg));
        f.render_widget(
            Paragraph::new(label)
                .style(text)
                .alignment(Alignment::Center)
                .block(block),
            area,
        );
    }

    fn draw_buttons(&mut self, f: &mut Frame, area: Rect) {
        let p = self.pal;
        let specs = [
            (Hit::Rescan, "[R] Rescan", p.gold, false),
            (Hit::Remove, "[X] Remove Licenses…", p.crimson, true),
            (Hit::Quit, "[Q] Quit", p.white, false),
        ];
        let mut x = area.x + 2;
        for (hit, label, accent, filled) in specs {
            let width = label.chars().count() as u16 + 4;
            if x + width > area.right() {
                break;
            }
            let r = Rect::new(x, area.y, width, 3);
            let enabled = hit == Hit::Quit || self.busy.is_none();
            self.button(f, r, label, accent, filled, enabled, false);
            if enabled && self.confirm.is_none() {
                self.hits.push((r, hit));
            }
            x += width + 2;
        }
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let p = self.pal;
        let key = Style::new().fg(p.gold).add_modifier(Modifier::BOLD);
        let desc = Style::new().fg(p.gold);
        let off = Style::new().fg(p.gray);
        let idle = self.busy.is_none();
        let items: &[(&str, &str, bool)] = if self.confirm.is_some() {
            &[
                ("Esc", "Cancel", true),
                ("Tab", "Next", true),
                ("Enter", "Press", true),
            ]
        } else {
            &[
                ("r", "Rescan", idle),
                ("x", "Remove Licenses…", idle),
                ("q", "Quit", true),
                ("↑↓", "Scroll", true),
            ]
        };
        let mut spans = vec![Span::raw(" ")];
        for &(k, d, on) in items {
            spans.push(Span::styled(format!(" {k} "), if on { key } else { off }));
            spans.push(Span::styled(format!("{d}  "), if on { desc } else { off }));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn confirm_body(&self) -> String {
        let mut body = format!(
            "Word, Excel, PowerPoint, Outlook and OneNote will be force-quit. The Microsoft 365 / \
             Office 2021 / 2024 license files and the Office sign-in keychain items for {} will \
             be deleted.\n\nThis cannot be undone. You will have to sign in to Office again to \
             reactivate it.",
            self.scope.user
        );
        if self.scope.lab.is_some() {
            body.push_str(
                "\n\nLAB MODE: only the fixture tree changes. The app quit, keychain and \
                 cfprefsd steps are skipped.",
            );
        }
        body
    }

    fn draw_confirm(&mut self, f: &mut Frame, area: Rect) {
        let p = self.pal;
        let Some(c) = &self.confirm else {
            return;
        };
        let (input, focus) = (c.input.clone(), c.focus);
        // Dim the main screen behind the dialog.
        f.buffer_mut()
            .set_style(area, Style::new().add_modifier(Modifier::DIM));
        let width = 76.min(area.width.saturating_sub(4));
        let body = word_wrap(&self.confirm_body(), width.saturating_sub(6) as usize);
        let height = (body.len() as u16 + 14).min(area.height);
        let r = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        f.render_widget(Clear, r);
        let block = Block::bordered()
            .border_type(BorderType::Thick)
            .border_style(Style::new().fg(p.crimson).bg(p.black))
            .style(Style::new().fg(p.white).bg(p.black));
        let inner = block.inner(r).inner(Margin::new(2, 1));
        f.render_widget(block, r);
        let [title, _, body_area, _, input_area, _, buttons] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(body.len() as u16),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .areas(inner);
        f.render_widget(
            Paragraph::new("Remove all Office licenses from this Mac?")
                .style(Style::new().fg(p.gold).add_modifier(Modifier::BOLD))
                .alignment(Alignment::Center),
            title,
        );
        f.render_widget(
            Paragraph::new(body.into_iter().map(Line::from).collect::<Vec<_>>())
                .style(Style::new().fg(p.white)),
            body_area,
        );

        let typing = focus == Focus::Input;
        let field = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if typing { p.gold } else { p.white }));
        let field_inner = field.inner(input_area);
        let shown = if input.is_empty() {
            Line::styled("Type REMOVE to confirm", Style::new().fg(p.gray))
        } else {
            Line::styled(input.clone(), Style::new().fg(p.white))
        };
        f.render_widget(Paragraph::new(shown).block(field), input_area);
        if typing && field_inner.width > 0 {
            let x = (field_inner.x + input.chars().count() as u16).min(field_inner.right() - 1);
            f.set_cursor_position(Position::new(x, field_inner.y));
        }
        self.hits.push((input_area, Hit::Input));

        // Cancel and Remove, right-aligned.
        let w = 10;
        let remove = Rect::new(buttons.right().saturating_sub(w), buttons.y, w, 3);
        let cancel = Rect::new(remove.x.saturating_sub(w + 2), buttons.y, w, 3);
        self.button(
            f,
            cancel,
            "Cancel",
            p.white,
            false,
            true,
            focus == Focus::Cancel,
        );
        self.button(
            f,
            remove,
            "Remove",
            p.crimson,
            true,
            true,
            focus == Focus::Remove,
        );
        self.hits.push((cancel, Hit::Cancel));
        self.hits.push((remove, Hit::ConfirmRemove));
    }
}

// ---------------------------------------------------------------- helpers

fn exit_text(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("exit {c}"),
        None => "stopped by a signal".into(),
    }
}

/// Replace control characters, so CLI output cannot send terminal escapes.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Split `text` into rows of at most `width` characters.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || width == 0 {
        return vec![String::new()];
    }
    chars.chunks(width).map(|c| c.iter().collect()).collect()
}

/// Greedy word wrap. A blank line in `text` stays a blank row.
fn word_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for para in text.split('\n') {
        let mut row = String::new();
        for word in para.split_whitespace() {
            let mut chars: Vec<char> = word.chars().collect();
            // A word longer than a row is split.
            while chars.len() > width {
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
                rows.push(chars.drain(..width).collect());
            }
            if chars.is_empty() {
                continue;
            }
            let len = row.chars().count();
            if len > 0 && len + 1 + chars.len() > width {
                rows.push(std::mem::take(&mut row));
            }
            if !row.is_empty() {
                row.push(' ');
            }
            row.extend(chars);
        }
        rows.push(row);
    }
    rows
}

/// Run `cmd` with stdout and stderr on one pipe; returns the exit code (None
/// when it was stopped by a signal or could not start) and the output.
fn run_merged(mut cmd: Command) -> (Option<i32>, String) {
    let (mut reader, writer) = match io::pipe() {
        Ok(pipe) => pipe,
        Err(e) => return (None, format!("could not create a pipe: {e}")),
    };
    let stdout = match writer.try_clone() {
        Ok(w) => w,
        Err(e) => return (None, format!("could not create a pipe: {e}")),
    };
    cmd.stdin(Stdio::null()).stdout(stdout).stderr(writer);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return (
                None,
                format!("could not start {}: {e}", cmd.get_program().display()),
            )
        }
    };
    // Close this process's write ends, so the read below ends at EOF.
    drop(cmd);
    let mut out = Vec::new();
    let _ = reader.read_to_end(&mut out);
    let code = child.wait().ok().and_then(|s| s.code());
    (code, String::from_utf8_lossy(&out).into_owned())
}

/// SHA-256 of `path` as 64 hex digits, from `/usr/bin/shasum`.
fn sha256_hex(path: &Path) -> Result<String, String> {
    let out = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("/usr/bin/shasum: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let hash = text.split(' ').next().unwrap_or_default();
    // Fail closed: the hash goes to the root script.
    if out.status.success() && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(hash.to_owned())
    } else {
        Err(format!(
            "could not compute the SHA-256 of {}",
            path.display()
        ))
    }
}

// ---------------------------------------------------------------- sudo

struct PrivRun {
    /// The root script printed ROOT_STARTED, so sudo accepted the password.
    started: bool,
    /// Ctrl-C reached this process while sudo ran.
    interrupted: bool,
    code: Option<i32>,
    lines: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    /// Nothing ran as root; the message says why.
    NotStarted(String),
    Finished {
        code: Option<i32>,
        interrupted: bool,
    },
}

fn outcome(run: &PrivRun) -> Outcome {
    if run.started {
        Outcome::Finished {
            code: run.code,
            interrupted: run.interrupted,
        }
    } else if run.interrupted {
        Outcome::NotStarted(PASSWORD_CANCELLED.into())
    } else {
        Outcome::NotStarted(format!(
            "sudo did not start the remove ({}). Nothing was changed.",
            exit_text(run.code)
        ))
    }
}

fn sudo_command(cli: &Path, user: &str, sha256: &str) -> Command {
    let mut c = Command::new("/usr/bin/sudo");
    // -f: root's zsh must not read the user's ~/.zshenv (sudo may keep HOME).
    c.args(["/bin/zsh", "-f", "-c", SUDO_REMOVE, "olr"])
        .arg(cli)
        .args([user, sha256]);
    c
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
const SIGINT: i32 = 2;
const SIG_DFL: usize = 0;

extern "C" fn on_sigint(_: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
}

/// Run `cmd` in the plain terminal. Ctrl-C reaches `cmd` (sudo) but only
/// sets a flag here, so this process stays alive to restore the UI.
fn run_privileged(cmd: Command) -> io::Result<PrivRun> {
    INTERRUPTED.store(false, Ordering::SeqCst);
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe { signal(SIGINT, on_sigint as extern "C" fn(i32) as usize) };
    let run = pump(cmd);
    // SAFETY: restores the default action.
    unsafe { signal(SIGINT, SIG_DFL) };
    let mut run = run?;
    run.interrupted = INTERRUPTED.load(Ordering::SeqCst);
    Ok(run)
}

/// Start `cmd` with stdout and stderr on one pipe. Echo each line to the
/// terminal and collect it. The ROOT_STARTED line is recorded, not shown.
fn pump(mut cmd: Command) -> io::Result<PrivRun> {
    let (reader, writer) = io::pipe()?;
    cmd.stdout(writer.try_clone()?).stderr(writer);
    let mut child = cmd.spawn()?;
    // Close this process's write ends, so the read below ends at EOF.
    drop(cmd);
    let mut run = PrivRun {
        started: false,
        interrupted: false,
        code: None,
        lines: Vec::new(),
    };
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf)
            .trim_end_matches(['\n', '\r'])
            .to_owned();
        if !run.started && line == ROOT_STARTED {
            run.started = true;
            continue;
        }
        println!("{line}");
        run.lines.push(line);
    }
    run.code = child.wait()?.code();
    Ok(run)
}

/// The remove under sudo, run while the UI is suspended.
fn sudo_remove(scope: &Scope, sha256: &str) -> io::Result<PrivRun> {
    println!(
        "\n  Removing Office licenses for {}. sudo asks for your password.\n",
        scope.user
    );
    let run = run_privileged(sudo_command(&scope.cli, &scope.user, sha256));
    // sudo can leave echo off when it is interrupted at the prompt.
    let _ = Command::new("/bin/stty").arg("sane").status();
    print!("\n  Press Enter to return…");
    let _ = io::stdout().flush();
    let _ = io::stdin().read_line(&mut String::new());
    run
}

// ---------------------------------------------------------------- terminal

fn enter_ui() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
}

fn leave_ui() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    )
}

fn event_loop(app: &mut App) -> io::Result<()> {
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    app.start(Job::Scan {
        after_remove: false,
    });
    while !app.quit {
        app.poll(Duration::ZERO);
        term.draw(|f| app.draw(f))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let action = match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
            Event::Mouse(m) => app.on_mouse(m),
            _ => Action::None,
        };
        if action == Action::SudoRemove {
            match sha256_hex(&app.scope.cli) {
                Err(e) => {
                    let msg = format!("Failed ({e}). Nothing was changed.");
                    app.note(&msg);
                    app.status = msg;
                }
                Ok(sha256) => {
                    leave_ui()?;
                    let result = sudo_remove(&app.scope, &sha256);
                    enter_ui()?;
                    term.clear()?;
                    app.after_sudo(result);
                }
            }
        }
    }
    Ok(())
}

pub fn main(home: Option<PathBuf>, system_root: Option<PathBuf>, smoke: bool) -> ExitCode {
    let cli = match std::env::current_exe().and_then(std::fs::canonicalize) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot locate this program: {e}");
            return ExitCode::FAILURE;
        }
    };
    let user = match sys::user_name() {
        Some(u) if sys::valid_user_name(&u) => u,
        _ => {
            eprintln!("error: could not read a valid user name from /usr/bin/id -un");
            return ExitCode::FAILURE;
        }
    };
    if smoke {
        return smoke::run(cli, user);
    }
    // Like the native app, the UI also takes lab paths from the environment.
    let env_path = |name| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let (home, system_root) = match (home, system_root) {
        (None, None) => (env_path("OLR_LAB_HOME"), env_path("OLR_LAB_SYSTEM_ROOT")),
        given => given,
    };
    // Half a lab setup must not fall through to a real remove.
    let lab = match (home, system_root) {
        (Some(h), Some(r)) => Some((h, r)),
        (None, None) => None,
        _ => {
            eprintln!("error: lab mode needs both --home and --system-root (or both OLR_LAB_HOME and OLR_LAB_SYSTEM_ROOT)");
            return ExitCode::FAILURE;
        }
    };
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("error: the terminal UI needs a terminal. Use `scan` or `remove` in scripts.");
        return ExitCode::FAILURE;
    }

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = leave_ui();
        prev(info);
    }));
    let mut app = App::new(Scope { cli, user, lab }, Palette::detect());
    let result = enter_ui().and_then(|()| event_loop(&mut app));
    let restored = leave_ui();
    match result.and(restored) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: terminal UI: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(lab: bool) -> App {
        let lab = lab.then(|| {
            (
                PathBuf::from("/nonexistent/h"),
                PathBuf::from("/nonexistent/r"),
            )
        });
        let scope = Scope {
            cli: PathBuf::from("/nonexistent/office-license-removal"),
            user: "bob".into(),
            lab,
        };
        App::new(scope, Palette::TRUE_COLOR)
    }

    fn press(app: &mut App, code: KeyCode) -> Action {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            assert_eq!(press(app, KeyCode::Char(ch)), Action::None);
        }
    }

    #[test]
    fn only_exact_remove_on_the_remove_button_asks_for_sudo() {
        let mut a = app(false);
        // Return in the field cancels, even with the right text.
        press(&mut a, KeyCode::Char('x'));
        type_text(&mut a, "REMOVE");
        assert_eq!(press(&mut a, KeyCode::Enter), Action::None);
        assert!(a.confirm.is_none());
        assert_eq!(a.status, CANCELLED);
        // Wrong text on the Remove button cancels.
        press(&mut a, KeyCode::Char('x'));
        type_text(&mut a, "remove");
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::Tab);
        assert_eq!(press(&mut a, KeyCode::Enter), Action::None);
        assert_eq!(a.status, CANCELLED);
        // Esc cancels.
        press(&mut a, KeyCode::Char('x'));
        type_text(&mut a, "REMOVE");
        assert_eq!(press(&mut a, KeyCode::Esc), Action::None);
        assert!(a.confirm.is_none());
        // REMOVE, then the Remove button.
        press(&mut a, KeyCode::Char('x'));
        type_text(&mut a, "REMOVE");
        press(&mut a, KeyCode::BackTab);
        assert_eq!(a.confirm.as_ref().map(|c| c.focus), Some(Focus::Remove));
        assert_eq!(press(&mut a, KeyCode::Enter), Action::SudoRemove);
        assert!(a.confirm.is_none());
    }

    #[test]
    fn keys_typed_in_the_dialog_do_not_run_commands() {
        let mut a = app(false);
        press(&mut a, KeyCode::Char('x'));
        type_text(&mut a, "qrx");
        assert!(!a.quit);
        assert!(a.busy.is_none());
        assert_eq!(a.confirm.as_ref().map(|c| c.input.as_str()), Some("qrx"));
    }

    #[test]
    fn remove_and_rescan_wait_for_a_running_job() {
        let mut a = app(false);
        let (_tx, rx) = mpsc::channel();
        a.busy = Some(rx);
        press(&mut a, KeyCode::Char('x'));
        assert!(a.confirm.is_none());
        press(&mut a, KeyCode::Char('r'));
        assert_eq!(a.status, "", "rescan started while a job ran");
        press(&mut a, KeyCode::Char('q'));
        assert!(a.quit);
    }

    #[test]
    fn outcome_reports_whether_root_code_ran() {
        let run = |started, interrupted, code| PrivRun {
            started,
            interrupted,
            code,
            lines: Vec::new(),
        };
        assert_eq!(
            outcome(&run(false, true, Some(1))),
            Outcome::NotStarted(PASSWORD_CANCELLED.into())
        );
        assert_eq!(
            outcome(&run(false, false, Some(1))),
            Outcome::NotStarted(
                "sudo did not start the remove (exit 1). Nothing was changed.".into()
            )
        );
        assert_eq!(
            outcome(&run(true, false, Some(0))),
            Outcome::Finished {
                code: Some(0),
                interrupted: false
            }
        );
        assert_eq!(
            outcome(&run(true, true, None)),
            Outcome::Finished {
                code: None,
                interrupted: true
            }
        );
    }

    #[test]
    fn sudo_command_pins_the_script_and_arguments() {
        assert!(SUDO_REMOVE.starts_with(&format!("print -r -- {ROOT_STARTED}\n")));
        let c = sudo_command(Path::new("/x/office-license-removal"), "bob", "ab12");
        assert_eq!(c.get_program(), "/usr/bin/sudo");
        let args: Vec<_> = c.get_args().collect();
        assert_eq!(
            args,
            [
                "/bin/zsh",
                "-f",
                "-c",
                SUDO_REMOVE,
                "olr",
                "/x/office-license-removal",
                "bob",
                "ab12"
            ]
        );
    }

    #[test]
    fn pump_records_the_start_marker_and_merges_stderr() {
        let mut c = Command::new("/bin/sh");
        c.args([
            "-c",
            "echo OLR_ROOT_STARTED; echo one; echo two >&2; exit 2",
        ]);
        let run = pump(c).unwrap();
        assert!(run.started);
        assert_eq!(run.lines, ["one", "two"]);
        assert_eq!(run.code, Some(2));

        let mut c = Command::new("/bin/sh");
        c.args(["-c", "echo 'Sorry, try again.' >&2; exit 1"]);
        let run = pump(c).unwrap();
        assert!(!run.started);
        assert_eq!(run.lines, ["Sorry, try again."]);
    }

    #[test]
    fn sha256_fails_closed() {
        assert_eq!(
            sha256_hex(Path::new("/dev/null")).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(sha256_hex(Path::new("/nonexistent/office-license-removal")).is_err());
    }

    #[test]
    fn text_helpers() {
        assert_eq!(wrap("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(wrap("", 4), [""]);
        assert_eq!(
            word_wrap("one two three\n\nfour", 7),
            ["one two", "three", "", "four"]
        );
        assert_eq!(word_wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(sanitize("a\u{1b}[2Jb"), "a?[2Jb");
    }
}
