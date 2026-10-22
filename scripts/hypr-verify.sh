#!/usr/bin/env bash
# Hyprland visual-verification helper for vrc-photo-manager.
#
# The app window is launched on a dedicated workspace (default 9) without
# switching to it, moved to a chosen monitor (default DP-3), and captured with
# `grim -T` (foreign-toplevel capture). Captures never require showing the
# window on a monitor.
#
# Requirements: Hyprland (Lua config era, 0.56+), grim 1.5+ with -T support,
# jq, and a built vrc-photo-manager binary.
#
# Usage:
#   scripts/hypr-verify.sh start [--release] [--monitor DP-3] [--workspace 9] \
#                                [-- <app args...>]
#   scripts/hypr-verify.sh capture <output.png>
#   scripts/hypr-verify.sh press [--hidden] [--capture <out.png>] [--settle SEC]
#                                <combo>          # e.g. ctrl+b, ctrl+i, escape
#   scripts/hypr-verify.sh status
#   scripts/hypr-verify.sh show                 # reveal ws9 for manual viewing
#   scripts/hypr-verify.sh stop
#
# `press` briefly focuses ws9, sends the key, optionally captures, then restores
# the previous window/workspace and cursor. Pass --hidden to skip the brief
# switch (the key may then be processed with a delay because the window is not
# visible).
#
# Typical flow:
#   scripts/hypr-verify.sh start --demo --demo-reset --window-size 1280x820
#   scripts/hypr-verify.sh capture /tmp/opencode/ui-1280-expanded.png
#   scripts/hypr-verify.sh press ctrl+b
#   scripts/hypr-verify.sh capture /tmp/opencode/ui-1280-collapsed.png
#   scripts/hypr-verify.sh stop
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
STATE_DIR="${XDG_RUNTIME_DIR:-/tmp}/vrc-photo-manager"
STATE_FILE="$STATE_DIR/hypr-verify.json"

PROFILE="${VRC_PROFILE:-debug}"
BIN_OVERRIDE="${VRC_BIN:-}"
WORKSPACE="${VRC_VERIFY_WORKSPACE:-9}"
MONITOR="${VRC_VERIFY_MONITOR:-DP-3}"
TITLE="${VRC_VERIFY_TITLE:-VRC Photo Manager}"

RESTORE_ACTIVE=0
RESTORE_ADDR=""
RESTORE_WS=""
RESTORE_MON=""
RESTORE_X=""
RESTORE_Y=""
RESTORE_APP_MON=""
RESTORE_APP_WS=""

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

note() {
    printf '%s\n' "$*" >&2
}

usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "${BASH_SOURCE[0]}"
}

require_hyprland() {
    command -v hyprctl >/dev/null 2>&1 || die "hyprctl not found"
    command -v grim >/dev/null 2>&1 || die "grim not found"
    command -v jq >/dev/null 2>&1 || die "jq not found"
    [[ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]] || die "not running inside a Hyprland session"
    grim -h 2>&1 | grep -q -- '-T ' || die "this grim does not support -T (toplevel capture)"
    [[ "$WORKSPACE" =~ ^[0-9]+$ ]] || die "workspace must be numeric: $WORKSPACE"
    [[ "$MONITOR" =~ ^[A-Za-z0-9._:-]+$ ]] || die "invalid monitor name: $MONITOR"
    [[ -n "$TITLE" ]] || die "window title must not be empty"
}

state_dir() {
    mkdir -p "$STATE_DIR"
    chmod 700 "$STATE_DIR"
}

load_state() {
    [[ -f "$STATE_FILE" ]] || die "no active verification window; run: $0 start ..."
    ADDR=$(jq -r '.address' "$STATE_FILE")
    STABLE=$(jq -r '.stableId' "$STATE_FILE")
    PID=$(jq -r '.pid' "$STATE_FILE")
    WS=$(jq -r '.workspace' "$STATE_FILE")
    MON=$(jq -r '.monitor' "$STATE_FILE")
    TITLE=$(jq -r '.title' "$STATE_FILE")
    [[ -n "$ADDR" && "$ADDR" != "null" ]] || die "state file is missing an address"
    [[ -n "$STABLE" && "$STABLE" != "null" ]] || die "state file is missing a stableId"
}

window_exists() {
    hyprctl clients -j | jq -e --arg a "$ADDR" 'any(.[]; .address == $a)' >/dev/null
}

find_window() {
    hyprctl clients -j | jq -r --arg t "$TITLE" \
        'first(.[] | select(.title == $t) | [.address, .stableId, (.pid|tostring), .workspace.name, (.monitor|tostring)] | @tsv) // empty'
}

monitor_exists() {
    hyprctl monitors -j | jq -e --arg m "$1" 'any(.[]; .name == $m)' >/dev/null
}

workspace_windows() {
    hyprctl clients -j | jq -r --arg w "$1" '[.[] | select(.workspace.name == $w)] | length'
}

workspace_visible() {
    hyprctl monitors -j | jq -r --arg w "$1" '[.[] | select(.activeWorkspace.name == $w)] | length'
}

save_focus() {
    RESTORE_ACTIVE=1
    RESTORE_ADDR=$(hyprctl activewindow -j | jq -r '.address // empty')
    RESTORE_WS=$(hyprctl activeworkspace -j | jq -r '.id // empty')
    RESTORE_MON=$(hyprctl monitors -j | jq -r 'first(.[] | select(.focused) | .name) // empty')
    local pos
    pos=$(hyprctl cursorpos)
    RESTORE_X=${pos%%,*}
    RESTORE_Y=${pos#*, }
    RESTORE_Y=${RESTORE_Y# }
}

restore_focus() {
    ((RESTORE_ACTIVE)) || return 0
    RESTORE_ACTIVE=0
    local lua=""
    # Put the app's monitor back on the workspace it showed before the press.
    if [[ -n "${RESTORE_APP_MON:-}" && "${RESTORE_APP_MON}" != "${RESTORE_MON:-}" && "${RESTORE_APP_WS:-}" =~ ^[0-9]+$ ]]; then
        lua+="hl.dispatch(hl.dsp.focus({monitor = \"$RESTORE_APP_MON\"}));"
        lua+="hl.dispatch(hl.dsp.focus({workspace = $RESTORE_APP_WS}));"
    fi
    if [[ -n "${RESTORE_ADDR:-}" ]]; then
        lua+="hl.dispatch(hl.dsp.focus({window = \"address:$RESTORE_ADDR\"}));"
    else
        if [[ -n "${RESTORE_MON:-}" ]]; then
            lua+="hl.dispatch(hl.dsp.focus({monitor = \"$RESTORE_MON\"}));"
        fi
        if [[ "${RESTORE_WS:-}" =~ ^[0-9]+$ ]]; then
            lua+="hl.dispatch(hl.dsp.focus({workspace = $RESTORE_WS}));"
        fi
    fi
    if [[ -n "${RESTORE_X:-}" && -n "${RESTORE_Y:-}" ]]; then
        lua+="hl.dispatch(hl.dsp.cursor.move({x=$RESTORE_X, y=$RESTORE_Y}));"
    fi
    if [[ -n "$lua" ]]; then
        hyprctl eval "$lua" >/dev/null 2>&1 || true
    fi
}

window_size_from_args() {
    local -a args=("$@")
    local i
    for ((i = 0; i < ${#args[@]}; i++)); do
        if [[ "${args[$i]}" == "--window-size" ]] && ((i + 1 < ${#args[@]})); then
            printf '%s' "${args[$((i + 1))]}"
            return 0
        fi
    done
    return 0
}

# Quote an argument for `sh -c` (used inside the Hyprland exec rule).
shell_quote() {
    printf '%q' "$1"
}

cmd_start() {
    require_hyprland
    local profile="$PROFILE"
    local -a app_args=()
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --release)
                profile=release
                shift
                ;;
            --monitor)
                [[ $# -ge 2 ]] || die "--monitor needs a value"
                MONITOR=$2
                shift 2
                ;;
            --workspace)
                [[ $# -ge 2 ]] || die "--workspace needs a value"
                WORKSPACE=$2
                shift 2
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            --)
                shift
                app_args+=("$@")
                break
                ;;
            *)
                app_args+=("$1")
                shift
                ;;
        esac
    done

    [[ "$WORKSPACE" =~ ^[0-9]+$ ]] || die "workspace must be numeric: $WORKSPACE"
    [[ "$MONITOR" =~ ^[A-Za-z0-9._:-]+$ ]] || die "invalid monitor name: $MONITOR"

    local win_size
    win_size=$(window_size_from_args "${app_args[@]}")
    local bin="${BIN_OVERRIDE:-$ROOT/target/$profile/vrc-photo-manager}"
    [[ -x "$bin" ]] || die "binary not found: $bin (build with: cargo build -p vrc-photo-ui)"

    local existing
    existing=$(find_window)
    [[ -z "$existing" ]] || die "a '$TITLE' window already exists: ${existing%%$'\t'*}; run '$0 stop' first"

    local ws_windows ws_visible
    ws_windows=$(workspace_windows "$WORKSPACE")
    ws_visible=$(workspace_visible "$WORKSPACE")
    [[ "$ws_windows" == "0" ]] || die "workspace $WORKSPACE already has $ws_windows window(s); refusing to take it over"
    [[ "$ws_visible" == "0" ]] || die "workspace $WORKSPACE is currently visible; switch to another workspace before starting"

    if ! monitor_exists "$MONITOR"; then
        note "warning: monitor '$MONITOR' not found; leaving workspace $WORKSPACE on the focused monitor"
        MONITOR=""
    fi

    local cmd
    cmd=$(shell_quote "$bin")
    local arg
    for arg in "${app_args[@]}"; do
        cmd+=" $(shell_quote "$arg")"
    done
    # Escape for the Lua double-quoted string passed to `hyprctl eval`.
    local lua_cmd=${cmd//\\/\\\\}
    lua_cmd=${lua_cmd//\"/\\\"}

    state_dir
    rm -f "$STATE_FILE"

    hyprctl eval "hl.dispatch(hl.dsp.exec_cmd(\"[workspace $WORKSPACE silent] $lua_cmd\"))" >/dev/null

    local win=""
    local _
    for _ in $(seq 1 100); do
        win=$(find_window || true)
        [[ -n "$win" ]] && break
        sleep 0.1
    done
    [[ -n "$win" ]] || die "window '$TITLE' did not appear within 10s"

    local addr stable pid ws_name mon_id
    IFS=$'\t' read -r addr stable pid ws_name mon_id <<<"$win"
    [[ -n "$stable" && "$stable" != "null" ]] || die "Hyprland did not expose a stableId; grim -T cannot capture this window"

    if [[ -n "$MONITOR" ]]; then
        hyprctl eval "hl.dispatch(hl.dsp.workspace.move({workspace = $WORKSPACE, monitor = \"$MONITOR\"}))" >/dev/null
    fi

    if [[ "$win_size" =~ ^([0-9]+)[xX]([0-9]+)$ ]]; then
        local width=${BASH_REMATCH[1]}
        local height=${BASH_REMATCH[2]}
        local size=""
        hyprctl eval "hl.dispatch(hl.dsp.window.float({window = \"address:$addr\", action = \"set\"}))" >/dev/null
        hyprctl eval "hl.dispatch(hl.dsp.window.resize({window = \"address:$addr\", x = $width, y = $height, relative = false}))" >/dev/null
        hyprctl eval "hl.dispatch(hl.dsp.window.center({window = \"address:$addr\"}))" >/dev/null
        local _
        for _ in $(seq 1 30); do
            size=$(hyprctl clients -j | jq -r --arg a "$addr" '.[] | select(.address == $a) | "\(.size[0])x\(.size[1])"')
            [[ "$size" == "${width}x${height}" ]] && break
            sleep 0.1
        done
        [[ "$size" == "${width}x${height}" ]] || note "warning: window size is $size, expected ${width}x${height}"
    fi

    # Give GPUI a moment to finish its first layout before keys/captures.
    sleep "${VRC_VERIFY_START_SETTLE:-1}"

    jq -n \
        --arg address "$addr" \
        --arg stableId "$stable" \
        --arg pid "$pid" \
        --arg workspace "$WORKSPACE" \
        --arg monitor "$MONITOR" \
        --arg title "$TITLE" \
        '{address:$address, stableId:$stableId, pid:$pid, workspace:$workspace, monitor:$monitor, title:$title}' \
        >"$STATE_FILE"

    printf 'started: pid=%s address=%s stableId=%s\n' "$pid" "$addr" "$stable"
    printf 'workspace: %s%s\n' "$WORKSPACE" "${MONITOR:+ ($MONITOR)}"
    printf 'capture:   %s capture /tmp/opencode/ui.png\n' "$0"
    printf 'press:     %s press ctrl+b\n' "$0"
    printf 'stop:      %s stop\n' "$0"
}

cmd_capture() {
    require_hyprland
    local out="${1:-}"
    [[ -n "$out" ]] || die "usage: $0 capture <output.png>"
    load_state
    window_exists || die "window $ADDR is gone; run '$0 status' or '$0 start ...'"
    mkdir -p "$(dirname "$out")"
    grim -T "$STABLE" "$out"
    printf '%s\n' "$out"
}

send_key() {
    local combo="$1"
    local -a parts=()
    IFS='+' read -r -a parts <<<"$combo"
    local key="${parts[${#parts[@]}-1]}"
    local -a mods=()
    local i
    for ((i = 0; i < ${#parts[@]} - 1; i++)); do
        case "${parts[$i],,}" in
            ctrl|control) mods+=(CTRL) ;;
            shift) mods+=(SHIFT) ;;
            alt) mods+=(ALT) ;;
            super|meta|win|cmd|command) mods+=(SUPER) ;;
            *) die "unknown modifier: ${parts[$i]}" ;;
        esac
    done
    case "$key" in
        esc|escape) key=Escape ;;
        enter|return) key=Return ;;
        space) key=space ;;
        tab) key=Tab ;;
        left) key=Left ;;
        right) key=Right ;;
        up) key=Up ;;
        down) key=Down ;;
        plus) key=equal ;;
        minus) key=minus ;;
    esac
    [[ "$key" =~ ^[A-Za-z0-9_]+$ ]] || die "unsupported key: $key"

    local mods_str=""
    ((${#mods[@]})) && mods_str="${mods[*]}"

    if [[ -n "$mods_str" ]]; then
        hyprctl eval "hl.dispatch(hl.dsp.send_shortcut({window = \"address:$ADDR\", mods = \"$mods_str\", key = \"$key\"}))" >/dev/null
    else
        hyprctl eval "hl.dispatch(hl.dsp.send_key_state({window = \"address:$ADDR\", mods = \"\", key = \"$key\", state = \"down\"}))" >/dev/null
        hyprctl eval "hl.dispatch(hl.dsp.send_key_state({window = \"address:$ADDR\", mods = \"\", key = \"$key\", state = \"up\"}))" >/dev/null
    fi
}

cmd_press() {
    require_hyprland
    local hidden=0
    local capture=""
    local settle="${VRC_VERIFY_SETTLE:-0.8}"
    local combo=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --hidden)
                hidden=1
                shift
                ;;
            --capture)
                [[ $# -ge 2 ]] || die "--capture needs a path"
                capture=$2
                shift 2
                ;;
            --settle)
                [[ $# -ge 2 ]] || die "--settle needs seconds"
                settle=$2
                shift 2
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            -*)
                die "unknown option: $1"
                ;;
            *)
                [[ -z "$combo" ]] || die "unexpected argument: $1"
                combo=$1
                shift
                ;;
        esac
    done
    [[ -n "$combo" ]] || die "usage: $0 press [--hidden] [--capture out.png] [--settle SEC] <combo>"
    load_state
    window_exists || die "window $ADDR is gone; run '$0 status' or '$0 start ...'"

    if ((hidden)); then
        send_key "$combo"
        sleep "$settle"
        [[ -n "$capture" ]] && cmd_capture "$capture" >/dev/null
    else
        save_focus
        local app_mon_id
        app_mon_id=$(hyprctl clients -j | jq -r --arg a "$ADDR" '.[] | select(.address == $a) | .monitor')
        if [[ -n "$app_mon_id" ]]; then
            RESTORE_APP_MON=$(hyprctl monitors -j | jq -r --argjson id "$app_mon_id" '.[] | select(.id == $id) | .name')
            RESTORE_APP_WS=$(hyprctl monitors -j | jq -r --argjson id "$app_mon_id" '.[] | select(.id == $id) | .activeWorkspace.name')
        fi
        trap restore_focus EXIT
        hyprctl eval "hl.dispatch(hl.dsp.focus({window = \"address:$ADDR\"}))" >/dev/null
        send_key "$combo"
        sleep "$settle"
        [[ -n "$capture" ]] && cmd_capture "$capture" >/dev/null
        restore_focus
        trap - EXIT
    fi
    printf 'sent %s to %s%s\n' "$combo" "$ADDR" "${capture:+ (captured $capture)}"
}

cmd_status() {
    require_hyprland
    if [[ ! -f "$STATE_FILE" ]]; then
        local win
        win=$(find_window || true)
        if [[ -n "$win" ]]; then
            printf 'window without state: %s\n' "$win"
        else
            printf 'not running\n'
        fi
        return 0
    fi
    load_state
    if window_exists; then
        printf 'running: pid=%s address=%s stableId=%s workspace=%s monitor=%s\n' \
            "$PID" "$ADDR" "$STABLE" "$WS" "$MON"
    else
        printf 'stale state: window %s is gone (run '\''%s stop'\'' to clear)\n' "$ADDR" "$0"
    fi
}

cmd_show() {
    require_hyprland
    load_state
    window_exists || die "window $ADDR is gone"
    hyprctl eval "hl.dispatch(hl.dsp.focus({window = \"address:$ADDR\"}))" >/dev/null
    printf 'focused %s (workspace %s%s)\n' "$ADDR" "$WS" "${MON:+ on $MON}"
}

cmd_stop() {
    require_hyprland
    if [[ ! -f "$STATE_FILE" ]]; then
        printf 'nothing to stop\n'
        return 0
    fi
    load_state
    if window_exists; then
        hyprctl eval "hl.dispatch(hl.dsp.window.close({window = \"address:$ADDR\"}))" >/dev/null || true
        local _
        for _ in $(seq 1 50); do
            window_exists || break
            sleep 0.1
        done
        if window_exists; then
            kill "$PID" 2>/dev/null || true
            sleep 0.5
            window_exists && kill -9 "$PID" 2>/dev/null || true
        fi
    fi
    rm -f "$STATE_FILE"
    printf 'stopped\n'
}

case "${1:-help}" in
    start)
        shift
        cmd_start "$@"
        ;;
    capture)
        shift
        cmd_capture "$@"
        ;;
    press)
        shift
        cmd_press "$@"
        ;;
    status)
        cmd_status
        ;;
    show)
        cmd_show
        ;;
    stop)
        cmd_stop
        ;;
    help|-h|--help)
        usage
        ;;
    *)
        die "unknown command: $1 (try '$0 help')"
        ;;
esac
