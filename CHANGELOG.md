# Changelog

## Unreleased (2026-10-21)

### Terminal UI

- The launcher now installs its own dependencies. It uses a private environment in `~/Library/Application Support/office-license-removal/venv`, or `~/.venv` when that has Textual 8. If neither works, it installs Textual 8.2.8 into the private environment. This step needs an internet connection.
- Before the UI starts, the launcher checks for Python 3.10 or later and checks every import that the UI uses.
- The new option `--setup-deps` installs the dependencies and exits. It does not scan license data.
- `--self-test` stays read-only. When a dependency is missing, it stops and names `--setup-deps`.
- The launcher never changes the system Python. It stops when the private environment folder exists without a working Python, and it does not replace that folder.

### Build

- `scripts/build-app.sh` signs the app in a staging folder outside the repo and verifies it there, then moves it into `dist/`. iCloud Drive adds Finder attributes to a bundle in a synced folder such as `~/Documents`, and `codesign` refuses such a bundle.

## 0.1.0 (2026-10-21)

This is the first release of a Rust replacement for the postinstall script in Microsoft's "Office for Mac License Removal Tool 2.7". It includes a native macOS app and a terminal UI around the same command-line tool.

### Why

Two faults in the 2.7 script made parts of it fail without an error:

- Its flight reset never ran. The script is zsh, and zsh rejects `[ x == y ]`. So every registry lookup returned an empty string, and every SQL statement failed.
- Its keychain deletes ran in the securityd session of root. The login keychain of the user stays locked in that session. So the sign-in items remained in the keychain.

### Command-line tool (`office-license-removal`)

- `scan` reports which license files, keychain items, flight rows and preference files exist. It changes nothing and never reads keychain passwords.
- `remove` renames the perpetual license plist to `.Backup` (or `.Backup.<timestamp>` when one exists). It deletes the Office 2021/2024 perpetual folder, the Microsoft 365 subscription files, the `Licenses` folder and 15 Office keychain items. It clears the "belongs to" preferences and the per-app first-run flags. It deletes the ECS flight rows in `MicrosoftRegistrationDB.reg` and restarts cfprefsd.
- Keychain and `defaults` commands run in the session of the user (`launchctl asuser … sudo -u`). The login keychain stays unlocked in that session.
- Each keychain delete repeats until no item matches. Exit 44 from `security` means "not found". Any other failure appears in the output and makes the run fail.
- Registry edits use parameterised SQL in one immediate transaction.
- A path guard protects every delete, rename, registry open and preference write. It refuses empty paths, `/`, `..`, anything outside the resolved home or system root, and any symlinked folder on the way to the target.
- The tool reports a path that it cannot inspect (for example, permission denied) as an error, never as absent.
- If the tool cannot resolve an explicit `--user`, it stops before any change.
- The tool quits the Office apps of the target user only, with `pkill -HUP -u <uid>`. Then it waits up to 5 seconds.
- Lab mode (`--home` and `--system-root`) works on a fixture tree without root, for testing.
- Exit codes: 0 done, 1 refused or aborted, 2 finished with a failed step.

### Native app (`Office License Removal.app`)

- A Rust and AppKit window shows the scan at launch and after every remove.
- The window is dark. It has a crimson header with a gold title and the status line, and white output on black.
- `Remove Licenses…` asks you to type `REMOVE`. Then it asks for administrator rights through the standard macOS prompt.
- The elevated shell copies the command-line tool into a root-only temporary folder. It compares the SHA-256 hash of the copy with the hash from the confirmation. Then it runs the copy. The shell refuses a changed binary.
- While a run is in progress, Quit and the window close button do nothing.
- `--ui-test <dir> [--flow scan|cancel|remove]` operates the real window without a human. It writes PNG captures and an events log. The cancel and remove flows need lab mode.

### Terminal UI (`Office License Removal.command`)

- A Textual (Python) interface has the same flows, the keys `r`, `x` and `q`, and a typed `REMOVE` confirmation.
- It opens in a kitty window if `/Applications/kitty.app` exists.
- The remove runs under `sudo` in the terminal with the same copy-verify-run protection. Ctrl-C at the password prompt returns to the UI.
- Python runs in isolated mode (`-I`), so the current folder and `PYTHONPATH` cannot inject modules.
- `--self-test` checks dependencies. `--smoke` runs a headless cancel and remove against a fixture.

### Build

- `scripts/build-app.sh` builds the tool and the app for arm64 and x86_64 and joins them with `lipo`. It assembles the bundle, signs it ad hoc and verifies it.
- `reference/postinstall-2.7.1-local` is the corrected shell script the tool follows step for step.
