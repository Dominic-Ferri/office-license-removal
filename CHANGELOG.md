# Changelog

## 0.1.0 (2026-10-21)

First release. A Rust replacement for the postinstall script in Microsoft's "Office for Mac License Removal Tool 2.7". It ships with a native macOS app and a terminal UI around the same command-line tool.

### Why

Two faults in the 2.7 script made parts of it fail silently:

- Its flight reset never ran. The script is zsh, and zsh rejects `[ x == y ]`, so every registry lookup returned an empty string and every SQL statement failed.
- Its keychain deletes ran in root's securityd session, where the user's login keychain is locked, so the sign-in items stayed behind.

### Command-line tool (`office-license-removal`)

- `scan` reports which license files, keychain items, flight rows and preference files exist. It changes nothing and never reads keychain passwords.
- `remove` performs the removal. It renames the perpetual license plist to `.Backup` (or `.Backup.<timestamp>` when one exists) and deletes the Office 2021/2024 perpetual folder. It deletes the Microsoft 365 subscription files, the `Licenses` folder and 15 Office keychain items. It clears the "belongs to" preferences and the per-app first-run flags, deletes the ECS flight rows in `MicrosoftRegistrationDB.reg`, and restarts cfprefsd.
- Keychain and `defaults` commands run inside the user's own session (`launchctl asuser … sudo -u`), so the login keychain is unlocked. Each keychain delete repeats until no item matches. Exit 44 from `security` means "not found". Any other failure is reported and fails the run.
- Registry edits use parameterised SQL in one immediate transaction.
- A path guard protects every delete, rename, registry open and preference write. It refuses empty paths, `/`, `..`, anything outside the resolved home or system root, and any symlinked folder on the way to the target.
- A path the tool cannot inspect (for example, permission denied) is reported as an error, never as absent.
- An explicit `--user` that cannot be resolved stops the run before any change.
- Office apps are quit with `pkill -HUP -u <uid>` for the target user only, followed by a 5-second wait.
- Lab mode (`--home` and `--system-root`) runs against a fixture tree without root for testing.
- Exit codes: 0 done, 1 refused or aborted, 2 finished with a failed step.

### Native app (`Office License Removal.app`)

- Rust + AppKit window. Shows the scan at launch and after every remove. Dark window with a crimson header, gold title and status line, white output on black.
- `Remove Licenses…` asks for confirmation with a typed `REMOVE`, then elevates through the standard macOS administrator prompt.
- The elevated shell copies the command-line tool into a root-only temporary folder. It verifies the copy's SHA-256 against the hash taken at confirmation, then runs the copy. A changed binary is refused.
- Quit and the window close button are disabled while a run is in progress.
- `--ui-test <dir> [--flow scan|cancel|remove]` drives the real window without a human and writes PNG captures and an events log. The cancel and remove flows require lab mode.

### Terminal UI (`Office License Removal.command`)

- Textual (Python) interface with the same flows, keyboard shortcuts `r`, `x`, `q`, and a typed `REMOVE` confirmation. Opens in a kitty window when kitty is installed.
- The remove runs under `sudo` in the terminal with the same copy-verify-run protection. Ctrl-C at the password prompt returns to the UI.
- Python runs in isolated mode (`-I`), so the current folder and `PYTHONPATH` cannot inject modules.
- `--self-test` checks dependencies. `--smoke` runs a headless cancel and remove against a fixture.

### Build

- `scripts/build-app.sh` builds the tool and the app for arm64 and x86_64, joins them with `lipo`, assembles the bundle, and ad-hoc signs and verifies it.
- `reference/postinstall-2.7.1-local` is the corrected shell script the tool follows step for step.
