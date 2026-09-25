#!/bin/sh
# Claude Code hook: signal iTerm2 when Claude is waiting on the user.
#
#   iterm2-notify waiting   (Stop)          notification + badge + tab color + dock bounce
#   iterm2-notify attention (Notification)  badge + tab color + dock bounce
#   iterm2-notify clear     (UserPromptSubmit, PostToolUse)  undo badge + tab color
#
# The escape sequences go to /dev/tty, not stdout: Claude Code captures hook
# stdout, and cbox forwards the box's tty bytes verbatim to the host terminal.
# `attention` skips the desktop notification because Claude Code already sends
# one itself for Notification events in iTerm2 (preferredNotifChannel "auto").
#
# No-op outside iTerm2 (cbox forwards TERM_PROGRAM/ITERM_SESSION_ID from the
# host terminal, per attach) or with CLAUDE_ITERM2_INTEGRATION=0.

[ "${CLAUDE_ITERM2_INTEGRATION:-1}" = 0 ] && exit 0
[ "${TERM_PROGRAM:-}" = iTerm.app ] || [ -n "${ITERM_SESSION_ID:-}" ] || exit 0
# Hooks inherit Claude's controlling terminal; without one there's nowhere to write.
( : >/dev/tty ) 2>/dev/null || exit 0

# Hook input arrives on stdin; drain it so Claude Code never blocks writing it.
cat >/dev/null 2>&1

# One marker per iTerm2 session, so `clear` (which fires after every tool
# call) only writes to the terminal when there is something to undo.
marker="${TMPDIR:-/tmp}/claude-iterm2-$(printf '%s' "${ITERM_SESSION_ID:-default}" | tr -c 'A-Za-z0-9-' _)"

osc() { printf '\033]%s\007' "$1"; }

highlight() {
    osc "1337;SetBadgeFormat=$(printf '%s' "$1" | base64 | tr -d '\n')"
    osc '6;1;bg;red;brightness;230'
    osc '6;1;bg;green;brightness;140'
    osc '6;1;bg;blue;brightness;0'
    osc '1337;RequestAttention=once'
}

{
    case "${1:-}" in
        waiting)
            osc '9;Claude Code is waiting for your input'
            highlight 'waiting for input'
            : >"$marker"
            ;;
        attention)
            highlight 'needs attention'
            : >"$marker"
            ;;
        clear)
            [ -e "$marker" ] || exit 0
            osc '1337;SetBadgeFormat='
            osc '6;1;bg;*;default'
            rm -f "$marker"
            ;;
    esac
} 2>/dev/null >/dev/tty

exit 0
