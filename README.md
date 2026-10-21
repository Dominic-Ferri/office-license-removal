# office-license-removal

Rust replacement for the postinstall script in Microsoft's "Office for Mac License Removal Tool 2.7". It follows the corrected script in `reference/postinstall-2.7.1-local` step for step: same paths, same keychain items, same registry values.

Three front ends share one CLI:

- `office-license-removal`: the CLI (root crate).
- `dist/Office License Removal.app`: a native macOS window (crate `app/`).
- `dist/Office License Removal.command`: a Textual terminal UI (Python, `~/.venv`).

## What `remove` does

1. Quits the target user's Word, Excel, PowerPoint, Outlook and OneNote with `pkill -HUP -u <uid>`. It then waits up to 5 seconds and prints `WARNING: <app> still running` for each app that did not exit.
2. Renames `/Library/Preferences/com.microsoft.office.licensingV2.plist` to `com.microsoft.office.licensingV2.Backup`. If that backup exists, it uses `com.microsoft.office.licensingV2.Backup.<YYYYmmdd-HHMMSS>` instead.
3. Deletes `/Library/Microsoft/Office/Licenses` (Office 2021 and 2024 perpetual licenses).
4. Deletes the six Microsoft 365 subscription files and the `Licenses` folder in the target user's `~/Library/Group Containers/UBF8T346G9.Office`.
5. Deletes the 15 Office sign-in keychain items in the user's own security session (`launchctl asuser`). Each delete repeats until no item matches, up to 20 times.
6. Clears `OfficeActivationEmailAddress` and sets the first-run flags with `defaults`, run as the user.
7. Deletes the ECS flight and config-cache values in `MicrosoftRegistrationDB.reg` in one SQLite transaction.
8. Restarts `cfprefsd`.

Each step writes its result to stdout and to `logger -t OfficeLicenseRemoval`.

## Usage

```
office-license-removal scan   [--user <shortname>] [--home <dir>] [--system-root <dir>]
sudo office-license-removal remove [--user <shortname>] [--yes] [--skip-keychain] [--skip-flights] [--skip-prefs] [--skip-quit]
office-license-removal remove --home <dir> --system-root <dir> [--user <shortname>] [--yes]   # lab mode
```

- `scan` changes nothing. It checks keychain items by their attributes and never reads passwords. `--home` and `--system-root` have the same meaning and checks as on `remove`, so you can scan a fixture tree before a lab-mode remove.
- `remove` needs root. Without `--yes`, it prints the scan and continues only when you type `REMOVE`.
- The target user is `--user`, or else the console user. With no console user, only the machine-wide steps run. If `--user` names an invalid or unknown user, or a user with no home, `scan` and `remove` stop with exit 1 before any change.
- Lab mode (`--home` and `--system-root` both given) works on a fixture tree without root. It does not quit apps, does not touch the keychain and does not restart cfprefsd.
- Every delete, rename, registry open and `defaults` write passes a path guard. The guard refuses empty paths, `/`, `..` and anything outside the resolved home or system root. It also refuses a target when a folder between the canonical home (or system root) and the target is a symlink. It refuses a target that is itself a symlink to a path outside. A refused target prints `REFUSED …`.
- The tool never reports a path it cannot inspect (for example, permission denied) as absent. `remove` prints `FAILED to inspect …` and `scan` prints an `[error]` line. Keychain checks work the same way: only `security` exit 44 means "not found".

Exit codes: 0 = done. 1 = refused or aborted, including an unresolvable `--user`. 2 = finished, but a step failed or the guard refused a path.

## Build and test

```
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo build --release --workspace
scripts/build-app.sh
```

The tests use fixtures under `$TMPDIR` (`olr-unit-*`, `olr-it-*`). They keep these fixtures on disk and never delete them.

`scripts/build-app.sh` builds the CLI and the app for arm64 and x86_64 in release. It joins each pair with `lipo`, writes `Info.plist`, ad-hoc signs the bundle and verifies it. It also copies the same universal CLI to `dist/office-license-removal`.

## Terminal UI

`dist/Office License Removal.command` is a double-click terminal UI (Textual, run with `~/.venv/bin/python -I`). The CLI must sit next to it. It opens in a 100×44 kitty window when `/Applications/kitty.app` exists. Otherwise it resizes the current terminal. It runs `scan --user <you>` at launch and after every remove.

`[X] Remove Licenses…` opens a confirm dialog. Press Remove with the field reading exactly `REMOVE`. The UI then suspends and runs the remove under `sudo` in the terminal. Root copies the CLI into a new root-only folder `/private/tmp/olr.XXXXXX`, checks that the copy has the SHA-256 taken when you confirmed, and runs the copy. If the CLI changed in between, it prints `REFUSED: office-license-removal changed since confirmation`, exits 3 and changes nothing. Ctrl-C at the password prompt cancels the run and returns to the UI.

Lab mode works as for the app: set both `OLR_LAB_HOME` and `OLR_LAB_SYSTEM_ROOT`, and the title ends in `— LAB MODE`. Checks:

```
"dist/Office License Removal.command" --self-test   # dependencies only; prints SELF-TEST OK
"dist/Office License Removal.command" --smoke       # headless cancel + remove on a kept $TMPDIR fixture (olr-tui-smoke-*); prints SMOKE OK
```

## App

`dist/Office License Removal.app` is a native macOS window (Rust + AppKit, crate `app/`) around the same CLI. The build script bundles the CLI next to it in `Contents/MacOS/`. The window is always dark. It has a crimson header band with a gold title and the status line, white output on black, a gold `Rescan` button and a crimson `Remove Licenses…` button.

- At launch, and after every remove, it runs `scan --user <you>` and shows the output.
- `Remove Licenses…` asks for confirmation: press Remove and type `REMOVE`. The app then hashes the bundled CLI and runs the remove through `osascript … with administrator privileges`. macOS asks for an admin password. As root, the shell copies the CLI into a new root-only folder `/private/tmp/olr.XXXXXX`, checks the copy's SHA-256 against the confirmed hash, and runs the copy with `remove --yes --user <you>`. If the hashes differ, the log shows `REFUSED: office-license-removal changed since confirmation` and the status shows `Failed (exit 3)`. Cancelling either prompt changes nothing. The tool does not remove the `olr.*` folders afterwards.
- While a scan or remove runs, Quit and the window close button do nothing. The status line shows `Wait for the current run to finish.` for 3 s.
- Lab mode: when you set both `OLR_LAB_HOME` and `OLR_LAB_SYSTEM_ROOT`, the title ends in `— LAB MODE`. `scan` and `remove` then get `--home "$OLR_LAB_HOME" --system-root "$OLR_LAB_SYSTEM_ROOT"`, and the CLI runs directly without elevation. `open` does not pass environment variables, so start the executable itself:

  ```
  OLR_LAB_HOME=<fixture>/home OLR_LAB_SYSTEM_ROOT=<fixture>/root "dist/Office License Removal.app/Contents/MacOS/OfficeLicenseRemoval"
  ```

### UI test harness

`OfficeLicenseRemoval --ui-test <dir> [--flow scan|cancel|remove]` drives the real window without a human. It never activates the app and never orders the window front. It presses the same buttons a user would and renders PNG captures in-process, so it needs no Screen Recording permission. Output in `<dir>`: numbered PNGs, `menu.txt` (the menu bar) and `events.log` (status changes, captures, `PASS`/`FAIL`, with elapsed ms).

- `scan` (default): waits for the launch scan, captures it and exits 0. It also works outside lab mode, because scan changes nothing.
- `cancel`: opens the Remove alert, types `nope`, presses Remove, and expects `Cancelled; nothing was changed.` with nothing removed.
- `remove`: types `REMOVE`, presses Remove, waits for the remove and the rescan, and expects `Done`.

`cancel` and `remove` need lab mode (`OLR_LAB_HOME` and `OLR_LAB_SYSTEM_ROOT`). Without it they write `refused: …` and exit 3. Exit codes: 0 = pass, 2 = fail or timeout (120 s), 3 = refused, 64 = bad arguments.

## Signing

The app, the CLI and the launcher are ad-hoc signed, not notarized. A copy that arrives with a quarantine flag (downloaded, AirDropped or unzipped from mail) will not open until you clear it:

```
xattr -d com.apple.quarantine "Office License Removal.app"
```

## Not covered by the automated tests

Every automated test runs in lab mode under `$TMPDIR`. The root path is not exercised: the `launchctl asuser … sudo -u` keychain and `defaults` commands, `pkill`, the cfprefsd restart, the `osascript` and `sudo` prompts, and the kitty relaunch. Test those on a spare Mac before fleet use.
