#!/bin/zsh
# Double-click launcher: Textual front-end for office-license-removal.
# The CLI must sit next to this file. UI runs on ~/.venv/bin/python (Textual 8).
#   --self-test  dependency checks only; never touches license data
#   --smoke      headless UI test against a new lab fixture in $TMPDIR

set -o nounset

PY="$HOME/.venv/bin/python"
CLI="${0:A:h}/office-license-removal"

die() {
  printf "\n  \033[38;2;192;0;39m✖\033[0m %s\n" "$1" >&2
  if [[ -t 0 ]]; then
    printf "\n  Press Enter to close"
    read -r _
  fi
  exit 1
}

[[ -x "$PY" ]] || die "Python venv not found at ~/.venv — run: python3 -m venv ~/.venv && ~/.venv/bin/pip install textual"
"$PY" -I -c 'import textual' 2>/dev/null || die "Textual not installed in ~/.venv — run: ~/.venv/bin/pip install textual"
[[ -x "$CLI" ]] || die "office-license-removal not found at $CLI (it must sit next to this .command file)"

if [[ "${1:-}" == "--self-test" ]]; then
  GLD='\033[38;2;244;189;71m'; DIM='\033[90m'; RST='\033[0m'
  cli_ver=$("$CLI" --version 2>&1) || die "$CLI --version failed: $cli_ver"
  printf "  ${DIM}›${RST} zsh %s\n" "$ZSH_VERSION"
  printf "  ${DIM}›${RST} %s\n" "$("$PY" --version 2>&1)"
  printf "  ${DIM}›${RST} Textual %s\n" "$("$PY" -I -c 'import textual; print(textual.__version__)')"
  printf "  ${DIM}›${RST} %s (%s)\n" "$cli_ver" "$CLI"
  printf "  ${GLD}✔${RST} SELF-TEST OK\n"
  exit 0
fi

# The app is passed to Python with -c, so no temp file is written or removed.
IFS= read -r -d '' PYSRC <<'PYEOF'
import asyncio
import hashlib
import os
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

from rich.text import Text
from textual import on, work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.screen import ModalScreen
from textual.theme import Theme
from textual.widgets import Button, Footer, Input, Label, RichLog, Static

TITLE = "Office License Removal"
PASSWORD_CANCELLED = "Cancelled at the password prompt; nothing was changed."
# Runs as root with $1 = CLI path, $2 = user, $3 = SHA-256 taken at confirmation.
# Root must not run the CLI from a folder the user can write to, so it copies
# the CLI into a new root-only temp dir (left in place), checks the copy's
# hash and runs the copy.
SUDO_REMOVE = r"""
t=$(/usr/bin/mktemp -d /private/tmp/olr.XXXXXX) || exit 1
/bin/cp "$1" "$t/office-license-removal" || exit 1
h=$(/usr/bin/shasum -a 256 "$t/office-license-removal")
[[ ${h[1,64]} == "$3" ]] || { print -r -- 'REFUSED: office-license-removal changed since confirmation'; exit 3; }
exec "$t/office-license-removal" remove --yes --user "$2"
"""
CRIMSON = "#C00027"
GOLD = "#F4BD47"

THEME = Theme(
    name="crimson-gold",
    dark=True,
    primary=CRIMSON,
    secondary=GOLD,
    accent=GOLD,
    background="#000000",
    surface="#000000",
    panel="#000000",
    foreground="#FFFFFF",
    error=CRIMSON,
    warning=GOLD,
    success=GOLD,
)

# Literal hex: Textual's derived $primary/$secondary come out one step off
# (#BF0027, #F3BD47), so the exact palette is pinned here.
CSS = """
$crimson: #C00027;
$gold: #F4BD47;
Screen { background: #000000; }
#title {
    height: 3; content-align: center middle;
    background: $crimson; color: $gold; text-style: bold;
}
#log {
    margin: 1 1 0 1; border: round $crimson; border-title-color: $gold;
    background: #000000; background-tint: #000000 0%; color: #FFFFFF;
    scrollbar-color: $crimson; scrollbar-color-hover: $gold; scrollbar-color-active: $gold;
    scrollbar-background: #000000; scrollbar-background-hover: #000000;
    scrollbar-background-active: #000000; scrollbar-corner-color: #000000;
}
#status { height: 1; margin: 0 2; color: $gold; text-style: bold; }
#buttons { height: auto; padding: 0 1; }
#buttons Button { margin: 0 1; }
#rescan { background: #000000; color: $gold; border: tall $gold; }
#remove, #confirm-remove { background: $crimson; color: #FFFFFF; border: tall $crimson; }
#quit, #cancel { background: #000000; color: #FFFFFF; border: tall #FFFFFF; }
#rescan:hover, #quit:hover, #cancel:hover { background: $crimson 15%; }
#remove:hover, #confirm-remove:hover { background: $crimson 70%; }
Footer, FooterKey { background: #000000; color: $gold; }
FooterKey .footer-key--key, FooterKey .footer-key--description {
    background: #000000; color: $gold;
}
ConfirmScreen { align: center middle; background: #000000 70%; }
#dialog {
    width: 76; height: auto; padding: 1 2;
    border: thick $crimson; background: #000000;
}
#dialog-title { width: 100%; content-align: center middle; color: $gold; text-style: bold; }
#dialog-body { margin: 1 0; color: #FFFFFF; }
#confirm-input {
    margin-bottom: 1; background: #000000; background-tint: #000000 0%;
    color: #FFFFFF; border: tall #FFFFFF;
}
#confirm-input:focus { border: tall $gold; }
#confirm-input > .input--cursor { background: $gold; color: #000000; }
#dialog-buttons { height: auto; align-horizontal: right; }
#dialog-buttons Button { margin-left: 2; }
"""


class ConfirmScreen(ModalScreen[bool]):
    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    def __init__(self, user: str, lab: bool) -> None:
        super().__init__()
        self.user = user
        self.lab = lab

    def compose(self) -> ComposeResult:
        body = (
            "Word, Excel, PowerPoint, Outlook and OneNote will be force-quit. "
            "The Microsoft 365 / Office 2021 / 2024 license files and the Office "
            f"sign-in keychain items for {self.user} will be deleted.\n\n"
            "This cannot be undone. You will have to sign in to Office again to reactivate it."
        )
        if self.lab:
            body += (
                "\n\nLAB MODE: only the fixture tree changes; app quit, keychain "
                "and cfprefsd steps are skipped."
            )
        with Vertical(id="dialog"):
            yield Label("Remove all Office licenses from this Mac?", id="dialog-title")
            yield Static(body, id="dialog-body", markup=False)
            yield Input(placeholder="Type REMOVE to confirm", id="confirm-input")
            with Horizontal(id="dialog-buttons"):
                yield Button("Cancel", id="cancel")
                yield Button("Remove", id="confirm-remove")

    def on_mount(self) -> None:
        self.query_one(Input).focus()

    @on(Button.Pressed, "#confirm-remove")
    def remove_pressed(self) -> None:
        self.dismiss(self.query_one(Input).value == "REMOVE")

    # Return in the field means Cancel, as Cancel is the default button.
    @on(Button.Pressed, "#cancel")
    @on(Input.Submitted)
    def action_cancel(self) -> None:
        self.dismiss(False)


class OfficeLicenseRemoval(App):
    TITLE = TITLE
    CSS = CSS
    ENABLE_COMMAND_PALETTE = False
    BINDINGS = [
        Binding("r", "rescan", "Rescan"),
        Binding("x", "remove", "Remove Licenses…"),
        Binding("q", "quit", "Quit"),
    ]

    def __init__(self, cli: str, user: str, lab: tuple[str, str] | None) -> None:
        super().__init__()
        self.cli = cli
        self.user = user
        self.lab = lab
        self.busy = False

    def compose(self) -> ComposeResult:
        yield Static(TITLE + (" — LAB MODE" if self.lab else ""), id="title", markup=False)
        log = RichLog(id="log", markup=False, highlight=False, wrap=False)
        log.border_title = "Output"
        yield log
        yield Static("", id="status", markup=False)
        with Horizontal(id="buttons"):
            yield Button(r"\[R] Rescan", id="rescan")
            yield Button(r"\[X] Remove Licenses…", id="remove")
            yield Button(r"\[Q] Quit", id="quit")
        yield Footer()

    def on_mount(self) -> None:
        self.register_theme(THEME)
        self.theme = "crimson-gold"
        self.scan_job()

    def check_action(self, action: str, parameters: tuple) -> bool | None:
        if action in ("rescan", "remove") and (self.busy or isinstance(self.screen, ModalScreen)):
            return None
        return True

    def set_busy(self, busy: bool) -> None:
        self.busy = busy
        self.query_one("#rescan", Button).disabled = busy
        self.query_one("#remove", Button).disabled = busy
        self.refresh_bindings()

    def set_status(self, text: str) -> None:
        self.query_one("#status", Static).update(text)

    def note(self, text: str) -> None:
        self.query_one(RichLog).write(Text(text, style=f"bold {GOLD}"))

    def scope_args(self) -> list[str]:
        args = ["--user", self.user]
        if self.lab:
            args += ["--home", self.lab[0], "--system-root", self.lab[1]]
        return args

    async def run_cli(self, command: str, *extra: str) -> int:
        proc = await asyncio.create_subprocess_exec(
            self.cli, command, *extra, *self.scope_args(),
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        text = out.decode(errors="replace").rstrip("\n")
        if text:
            self.query_one(RichLog).write(Text(text))
        return proc.returncode

    async def scan(self, after_remove: bool) -> None:
        if after_remove:
            self.note("\nScan after remove:")
        else:
            self.query_one(RichLog).clear()
            self.set_status("Scanning…")
        code = await self.run_cli("scan")
        if code != 0:
            self.note(f"Scan failed (exit {code})")
        # Like the native app, the rescan after a remove keeps the remove's status.
        if not after_remove:
            self.set_status("Scan complete" if code == 0 else f"Scan failed (exit {code})")

    def finish_remove(self, code: int) -> None:
        self.note(f"Finished (exit {code})")
        self.set_status("Done" if code == 0 else f"Failed (exit {code})")

    @work(exclusive=True, group="cli")
    async def scan_job(self, after_remove: bool = False) -> None:
        self.set_busy(True)
        await self.scan(after_remove)
        self.set_busy(False)

    @work(exclusive=True, group="cli")
    async def lab_remove_job(self) -> None:
        self.set_busy(True)
        self.note("\nRemove:")
        self.set_status("Removing…")
        self.finish_remove(await self.run_cli("remove", "--yes"))
        await self.scan(after_remove=True)
        self.set_busy(False)

    def sudo_remove(self) -> None:
        sha256 = hashlib.sha256(Path(self.cli).read_bytes()).hexdigest()
        self.note("\nRemove (sudo, output shown in the terminal):")
        with self.suspend():
            print(f"\n  Removing Office licenses for {self.user}. sudo asks for your password.\n")
            code = 130
            try:
                # -f: root's zsh must not read the user's ~/.zshenv (sudo may keep HOME).
                code = subprocess.run(
                    ["/usr/bin/sudo", "/bin/zsh", "-f", "-c", SUDO_REMOVE, "olr",
                     self.cli, self.user, sha256]
                ).returncode
                os.system("stty sane")
                input("\n  Press Enter to return…")
            except KeyboardInterrupt:
                if code == 130:
                    print("\n  Cancelled.")
        if code == 130:
            self.note(PASSWORD_CANCELLED)
            self.set_status(PASSWORD_CANCELLED)
            return
        self.finish_remove(code)
        self.scan_job(after_remove=True)

    def confirmed(self, ok: bool | None) -> None:
        if not ok:
            self.set_status("Cancelled; nothing was changed.")
        elif self.lab:
            self.lab_remove_job()
        else:
            self.sudo_remove()

    @on(Button.Pressed, "#rescan")
    def action_rescan(self) -> None:
        self.scan_job()

    @on(Button.Pressed, "#remove")
    def action_remove(self) -> None:
        self.push_screen(ConfirmScreen(self.user, self.lab is not None), self.confirmed)

    @on(Button.Pressed, "#quit")
    def quit_pressed(self) -> None:
        self.exit()


def make_app() -> OfficeLicenseRemoval:
    user = subprocess.run(
        ["/usr/bin/id", "-un"], capture_output=True, text=True, check=True
    ).stdout.strip()
    home = os.environ.get("OLR_LAB_HOME")
    root = os.environ.get("OLR_LAB_SYSTEM_ROOT")
    return OfficeLicenseRemoval(os.environ["OLR_CLI"], user, (home, root) if home and root else None)


GC = "Library/Group Containers/UBF8T346G9.Office"
NODES = [
    (1, -1, "Software"), (2, 1, "Microsoft"), (3, 2, "Office"), (4, 3, "16.0"),
    (5, 4, "Common"), (6, 5, "ExperimentEcs"), (7, 6, "excel"), (8, 7, "Flights"),
    (9, 5, "ExperimentConfigs"), (10, 9, "Ecs"), (11, 10, "powerpoint"),
    (12, 11, "ConfigContextData"), (13, 5, "General"),
]
# 7 flight/config rows the CLI deletes, then 3 unrelated rows it keeps
# (same data as seed_registry in tests/remove_fixture.rs).
VALUES = [
    (7, "ETag"), (7, "ConfigIds"), (8, "SomeFlight"), (11, "Expires"), (12, "ctx"),
    (13, "Url"), (13, "AuthorityUrl"),
    (7, "Keep"), (13, "ProductVersion"), (11, "Language"),
]


def value_rows(db: Path) -> int:
    c = sqlite3.connect(db)
    n = c.execute("SELECT COUNT(*) FROM HKEY_CURRENT_USER_values").fetchone()[0]
    c.close()
    return n


def build_fixture() -> tuple[Path, list[Path], Path, Path]:
    base = Path(tempfile.mkdtemp(prefix="olr-tui-smoke-"))
    root, home = base / "root", base / "home"
    perp = root / "Library/Preferences/com.microsoft.office.licensingV2.plist"
    perp.parent.mkdir(parents=True)
    perp.write_text(
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" '
        '"http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
        '<plist version="1.0"><dict><key>perpetual</key><string>fixture</string></dict></plist>\n'
    )
    licenses = [root / "Library/Microsoft/Office/Licenses", home / GC / "Licenses"]
    for lic in licenses:
        (lic / "5").mkdir(parents=True)
        (lic / "5/x").write_text("x")
    reg = home / GC / "MicrosoftRegistrationDB/MicrosoftRegistrationDB_fixture.reg"
    reg.parent.mkdir(parents=True)
    c = sqlite3.connect(reg)
    c.executescript(
        "CREATE TABLE HKEY_CURRENT_USER (node_id INTEGER PRIMARY KEY ASC, parent_id, name TEXT COLLATE nocase, write_time, UNIQUE (parent_id, name));"
        "CREATE TABLE HKEY_CURRENT_USER_values (node_id INTEGER, name TEXT COLLATE nocase, type INTEGER, value, UNIQUE (node_id, name));"
    )
    c.executemany("INSERT INTO HKEY_CURRENT_USER VALUES (?, ?, ?, 0)", NODES)
    c.executemany("INSERT INTO HKEY_CURRENT_USER_values VALUES (?, ?, 1, 'v')", VALUES)
    c.commit()
    c.close()
    (home / GC / "MicrosoftRegistrationDB.reg").symlink_to(reg)
    os.environ["OLR_LAB_HOME"] = str(home)
    os.environ["OLR_LAB_SYSTEM_ROOT"] = str(root)
    return base, licenses, perp, reg


async def smoke() -> None:
    base, licenses, perp, reg = build_fixture()
    print(f"fixture: {base}")
    app = make_app()

    def log_text() -> str:
        return "\n".join(line.text for line in app.query_one(RichLog).lines)

    def status() -> str:
        return str(app.query_one("#status", Static).render())

    async with app.run_test(size=(100, 42)) as pilot:
        async def settle() -> None:
            await pilot.pause()
            await asyncio.wait_for(app.workers.wait_for_complete(), 60)
            await pilot.pause()

        await settle()
        assert app.theme == "crimson-gold", f"theme is {app.theme}"
        title = str(app.query_one("#title", Static).render())
        assert title == "Office License Removal — LAB MODE", f"title is {title!r}"
        assert status() == "Scan complete", f"status after scan: {status()!r}"
        needle = "registry: 7 flight/config value row(s) would be deleted"
        assert needle in log_text(), f"scan log lacks {needle!r}:\n{log_text()}"
        print("ok: first scan")

        await pilot.press("x")
        await pilot.pause()
        assert isinstance(app.screen, ConfirmScreen), "x did not open the confirm dialog"
        await pilot.press(*"nope")
        await pilot.click("#confirm-remove")
        await settle()
        assert status() == "Cancelled; nothing was changed.", f"status after nope: {status()!r}"
        assert perp.exists(), "perpetual plist changed after cancel"
        assert all(lic.exists() for lic in licenses), "Licenses changed after cancel"
        assert value_rows(reg) == 10, f"registry rows after cancel: {value_rows(reg)}"
        print("ok: wrong confirmation cancelled, fixture untouched")

        await pilot.press("x")
        await pilot.pause()
        await pilot.press(*"REMOVE")
        await pilot.click("#confirm-remove")
        await settle()
        assert status() == "Done", f"status after remove: {status()!r}\n{log_text()}"
        for needle in (
            "Removed all Office flighting data (7 value row(s) deleted)",
            "Completed Office for Mac License Removal",
            "Finished (exit 0)",
            "Scan after remove:",
        ):
            assert needle in log_text(), f"log lacks {needle!r}:\n{log_text()}"
        gone = [str(lic) for lic in licenses if lic.exists()]
        assert not gone, f"still present: {gone}"
        assert value_rows(reg) == 3, f"registry rows after remove: {value_rows(reg)}"
        print("ok: REMOVE removed Licenses, registry has 3 rows")
    print("SMOKE OK")


if sys.argv[1:] == ["--smoke"]:
    try:
        asyncio.run(smoke())
    except AssertionError as e:
        print(f"SMOKE FAILED: {e}")
        sys.exit(1)
else:
    make_app().run()
PYEOF

export OLR_CLI="$CLI"

if [[ "${1:-}" == "--smoke" ]]; then
  exec "$PY" -I -c "$PYSRC" --smoke
fi

# Double-click opens Terminal.app (256 colours); relaunch in a sized kitty window.
KITTY_APP="/Applications/kitty.app"
if [[ -z "${KITTY_WINDOW_ID:-}" && "${TERM:-}" != "xterm-kitty" && -d "$KITTY_APP" ]]; then
  open -na "$KITTY_APP" --args --title "Office License Removal" \
    -o remember_window_size=no -o initial_window_width=100c -o initial_window_height=44c \
    /bin/zsh "$0" "$@"
  # Close only the Terminal window this script runs in, found by its tty.
  [[ "${TERM_PROGRAM:-}" == "Apple_Terminal" && -n "${TTY:-}" ]] && \
    ( sleep 0.3; osascript -e 'on run argv' -e 'tell application "Terminal"' \
        -e 'repeat with w in windows' \
        -e 'if tty of selected tab of w is item 1 of argv then close w' \
        -e 'end repeat' -e 'end tell' -e 'end run' "$TTY" >/dev/null 2>&1 ) &
  exit 0
fi
# Fallback resize for non-kitty terminals (kitty ignores this escape):
[[ -t 1 ]] && printf '\033[8;44;100t'

exec "$PY" -I -c "$PYSRC"
