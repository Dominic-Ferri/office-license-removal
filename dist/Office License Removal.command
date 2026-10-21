#!/bin/zsh
# Double-click launcher for the office-license-removal terminal UI.
# The CLI must sit next to this file. The UI is built into the CLI, so the
# launcher needs no Python and downloads nothing.
#   --self-test  checks only; never touches license data
#   --smoke      headless UI test against a new lab fixture in $TMPDIR

set -o nounset

CLI="${0:A:h}/office-license-removal"

die() {
  printf "\n  \033[38;2;192;0;39m✖\033[0m %s\n" "$1" >&2
  if [[ -t 0 ]]; then
    printf "\n  Press Enter to close"
    read -r _
  fi
  exit 1
}

[[ -x "$CLI" ]] || die "office-license-removal not found at $CLI (it must sit next to this .command file)"
"$CLI" tui --help >/dev/null 2>&1 || die "$CLI has no terminal UI. Use the office-license-removal file that was built with this launcher."

case "${1:-}" in
  --self-test)
    GLD='\033[38;2;244;189;71m'; DIM='\033[90m'; RST='\033[0m'
    cli_ver=$("$CLI" --version 2>&1) || die "$CLI --version failed: $cli_ver"
    printf "  ${DIM}›${RST} zsh %s\n" "$ZSH_VERSION"
    printf "  ${DIM}›${RST} %s (%s)\n" "$cli_ver" "$CLI"
    printf "  ${DIM}›${RST} terminal UI built in\n"
    printf "  ${GLD}✔${RST} SELF-TEST OK\n"
    exit 0
    ;;
  --smoke)
    exec "$CLI" tui --smoke
    ;;
esac

# Double-click opens Terminal.app; relaunch in a sized kitty window.
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

exec "$CLI" tui
