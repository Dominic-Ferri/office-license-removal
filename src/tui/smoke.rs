//! `tui --smoke`: drives the terminal UI headless (ratatui TestBackend)
//! against a new lab fixture in $TMPDIR. The fixture is kept, never deleted.

use super::{Action, App, Focus, Hit, Job, Palette, Scope, CANCELLED};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use rusqlite::{params, Connection};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

const GC: &str = "Library/Group Containers/UBF8T346G9.Office";
const NODES: [(i64, i64, &str); 13] = [
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
];
/// 7 flight/config rows the CLI deletes, then 3 unrelated rows it keeps
/// (same data as seed_registry in tests/remove_fixture.rs).
const VALUES: [(i64, &str); 10] = [
    (7, "ETag"),
    (7, "ConfigIds"),
    (8, "SomeFlight"),
    (11, "Expires"),
    (12, "ctx"),
    (13, "Url"),
    (13, "AuthorityUrl"),
    (7, "Keep"),
    (13, "ProductVersion"),
    (11, "Language"),
];
const PLIST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\"><dict><key>perpetual</key><string>fixture</string></dict></plist>\n";

struct Fixture {
    base: PathBuf,
    home: PathBuf,
    root: PathBuf,
    perp: PathBuf,
    licenses: [PathBuf; 2],
    reg: PathBuf,
}

fn ensure(ok: bool, msg: impl FnOnce() -> String) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(msg())
    }
}

fn build() -> Result<Fixture, String> {
    let io = |e: std::io::Error| e.to_string();
    let out = Command::new("/usr/bin/mktemp")
        .arg("-d")
        .arg(std::env::temp_dir().join("olr-tui-smoke-XXXXXX"))
        .output()
        .map_err(|e| format!("mktemp: {e}"))?;
    ensure(out.status.success(), || "mktemp failed".into())?;
    let base = fs::canonicalize(String::from_utf8_lossy(&out.stdout).trim()).map_err(io)?;
    let (root, home) = (base.join("root"), base.join("home"));
    let gc = home.join(GC);

    let prefs = root.join("Library/Preferences");
    fs::create_dir_all(&prefs).map_err(io)?;
    let perp = prefs.join("com.microsoft.office.licensingV2.plist");
    fs::write(&perp, PLIST).map_err(io)?;

    let licenses = [
        root.join("Library/Microsoft/Office/Licenses"),
        gc.join("Licenses"),
    ];
    for lic in &licenses {
        fs::create_dir_all(lic.join("5")).map_err(io)?;
        fs::write(lic.join("5/x"), "x").map_err(io)?;
    }

    let reg_dir = gc.join("MicrosoftRegistrationDB");
    fs::create_dir_all(&reg_dir).map_err(io)?;
    let reg = reg_dir.join("MicrosoftRegistrationDB_fixture.reg");
    let sql = |e: rusqlite::Error| e.to_string();
    let db = Connection::open(&reg).map_err(sql)?;
    db.execute_batch(
        "CREATE TABLE HKEY_CURRENT_USER (node_id INTEGER PRIMARY KEY ASC, parent_id, name TEXT COLLATE nocase, write_time, UNIQUE (parent_id, name));
         CREATE TABLE HKEY_CURRENT_USER_values (node_id INTEGER, name TEXT COLLATE nocase, type INTEGER, value, UNIQUE (node_id, name));",
    )
    .map_err(sql)?;
    for (id, parent, name) in NODES {
        db.execute(
            "INSERT INTO HKEY_CURRENT_USER VALUES (?1, ?2, ?3, 0)",
            params![id, parent, name],
        )
        .map_err(sql)?;
    }
    for (id, name) in VALUES {
        db.execute(
            "INSERT INTO HKEY_CURRENT_USER_values VALUES (?1, ?2, 1, 'v')",
            params![id, name],
        )
        .map_err(sql)?;
    }
    drop(db);
    std::os::unix::fs::symlink(&reg, gc.join("MicrosoftRegistrationDB.reg")).map_err(io)?;
    Ok(Fixture {
        base,
        home,
        root,
        perp,
        licenses,
        reg,
    })
}

fn value_rows(db: &Path) -> Result<i64, String> {
    Connection::open(db)
        .and_then(|c| {
            c.query_row("SELECT COUNT(*) FROM HKEY_CURRENT_USER_values", [], |r| {
                r.get(0)
            })
        })
        .map_err(|e| e.to_string())
}

fn untouched(fx: &Fixture) -> Result<(), String> {
    ensure(fx.perp.exists(), || {
        "perpetual plist changed after cancel".into()
    })?;
    ensure(fx.licenses.iter().all(|l| l.exists()), || {
        "Licenses changed after cancel".into()
    })?;
    let rows = value_rows(&fx.reg)?;
    ensure(rows == 10, || format!("registry rows after cancel: {rows}"))
}

/// Wait for the running job and any job it starts (a rescan after remove).
fn settle(app: &mut App) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while app.busy.is_some() {
        ensure(Instant::now() < deadline, || {
            "a CLI job did not finish within 60 s".into()
        })?;
        app.poll(Duration::from_millis(50));
    }
    Ok(())
}

fn draw(app: &mut App, term: &mut Terminal<TestBackend>) -> Result<(), String> {
    term.draw(|f| app.draw(f))
        .map(|_| ())
        .map_err(|e| format!("draw: {e:?}"))
}

fn screen(term: &Terminal<TestBackend>) -> Vec<String> {
    let buf = term.backend().buffer();
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

fn key(app: &mut App, code: KeyCode) -> Result<(), String> {
    let action = app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    ensure(action == Action::None, || {
        format!("lab mode returned {action:?}")
    })
}

fn type_text(app: &mut App, text: &str) -> Result<(), String> {
    text.chars().try_for_each(|c| key(app, KeyCode::Char(c)))
}

/// Left-click the centre of a clickable area from the last draw.
fn click(app: &mut App, hit: Hit) -> Result<(), String> {
    let r = app
        .hits
        .iter()
        .find(|(_, h)| *h == hit)
        .map(|(r, _)| *r)
        .ok_or_else(|| format!("{hit:?} is not on screen"))?;
    let action = app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: r.x + r.width / 2,
        row: r.y + r.height / 2,
        modifiers: KeyModifiers::NONE,
    });
    ensure(action == Action::None, || {
        format!("lab mode returned {action:?}")
    })
}

fn log_text(app: &App) -> String {
    app.log
        .iter()
        .map(|(t, _)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn steps(fx: &Fixture, cli: PathBuf, user: String) -> Result<(), String> {
    let pal = Palette::TRUE_COLOR;
    let scope = Scope {
        cli,
        user,
        lab: Some((fx.home.clone(), fx.root.clone())),
    };
    let mut app = App::new(scope, pal);
    let mut term =
        Terminal::new(TestBackend::new(100, 42)).map_err(|e| format!("terminal: {e:?}"))?;

    app.start(Job::Scan {
        after_remove: false,
    });
    settle(&mut app)?;
    draw(&mut app, &mut term)?;
    let rows = screen(&term);
    let title = "Office License Removal · LAB MODE";
    let col = rows[1]
        .find(title)
        .ok_or_else(|| format!("title row is {:?}", rows[1].trim()))?;
    let cell = &term.backend().buffer()[(col as u16, 1)];
    ensure(cell.fg == pal.gold && cell.bg == pal.crimson, || {
        format!("title colours are {:?} on {:?}", cell.fg, cell.bg)
    })?;
    ensure(app.status == "Scan complete", || {
        format!("status after scan: {:?}", app.status)
    })?;
    let needle = "registry: 7 flight/config value row(s) would be deleted";
    ensure(log_text(&app).contains(needle), || {
        format!("scan log lacks {needle:?}:\n{}", log_text(&app))
    })?;
    // The log follows new output, so the last scan line is on screen.
    let last: String = app
        .log
        .last()
        .map(|(t, _)| t.chars().take(40).collect())
        .unwrap_or_default();
    ensure(
        !last.is_empty() && rows.iter().any(|r| r.contains(&last)),
        || format!("the last scan line {last:?} is not on screen"),
    )?;
    println!("ok: first scan");

    key(&mut app, KeyCode::Char('x'))?;
    ensure(app.confirm.is_some(), || {
        "x did not open the confirm dialog".into()
    })?;
    type_text(&mut app, "nope")?;
    draw(&mut app, &mut term)?;
    let rows = screen(&term);
    ensure(
        rows.iter()
            .any(|r| r.contains("Remove all Office licenses from this Mac?"))
            && rows.iter().any(|r| r.contains("nope")),
        || "the confirm dialog is not on screen".into(),
    )?;
    click(&mut app, Hit::ConfirmRemove)?;
    settle(&mut app)?;
    ensure(app.confirm.is_none() && app.status == CANCELLED, || {
        format!("status after nope: {:?}", app.status)
    })?;
    untouched(fx)?;
    println!("ok: wrong confirmation cancelled, fixture untouched");

    key(&mut app, KeyCode::Char('x'))?;
    type_text(&mut app, "REMOVE")?;
    key(&mut app, KeyCode::Enter)?;
    settle(&mut app)?;
    ensure(app.confirm.is_none() && app.status == CANCELLED, || {
        format!("status after Return in the field: {:?}", app.status)
    })?;
    untouched(fx)?;
    println!("ok: Return in the field cancelled, fixture untouched");

    key(&mut app, KeyCode::Char('x'))?;
    type_text(&mut app, "REMOVE")?;
    key(&mut app, KeyCode::Tab)?;
    key(&mut app, KeyCode::Tab)?;
    ensure(
        app.confirm.as_ref().map(|c| c.focus) == Some(Focus::Remove),
        || "Tab Tab did not reach the Remove button".into(),
    )?;
    key(&mut app, KeyCode::Enter)?;
    settle(&mut app)?;
    draw(&mut app, &mut term)?;
    ensure(app.status == "Done", || {
        format!("status after remove: {:?}\n{}", app.status, log_text(&app))
    })?;
    for needle in [
        "Removed all Office flighting data (7 value row(s) deleted)",
        "Completed Office for Mac License Removal",
        "Finished (exit 0)",
        "Scan after remove:",
    ] {
        ensure(log_text(&app).contains(needle), || {
            format!("log lacks {needle:?}:\n{}", log_text(&app))
        })?;
    }
    let gone: Vec<_> = fx.licenses.iter().filter(|l| l.exists()).collect();
    ensure(gone.is_empty(), || format!("still present: {gone:?}"))?;
    let left = value_rows(&fx.reg)?;
    ensure(left == 3, || format!("registry rows after remove: {left}"))?;
    println!("ok: REMOVE removed Licenses, registry has 3 rows");

    term.backend_mut().resize(30, 10);
    draw(&mut app, &mut term)?;
    ensure(screen(&term).join(" ").contains("larger"), || {
        "a small window shows no resize hint".into()
    })?;
    println!("ok: small window shows a resize hint");

    key(&mut app, KeyCode::Char('q'))?;
    ensure(app.quit, || "q did not quit".into())
}

pub fn run(cli: PathBuf, user: String) -> ExitCode {
    let result = build().and_then(|fx| {
        println!("fixture: {}", fx.base.display());
        steps(&fx, cli, user)
    });
    match result {
        Ok(()) => {
            println!("SMOKE OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("SMOKE FAILED: {e}");
            ExitCode::FAILURE
        }
    }
}
