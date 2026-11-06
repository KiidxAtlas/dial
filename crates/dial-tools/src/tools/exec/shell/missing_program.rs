//! Recovery evidence from the failing Bash itself, never the controller's host PATH.
//!
//! Fixed alternatives use builtin lookup, never candidate execution. Bash 3 lacks the missing
//! command hook, so a conservative ERR trap handles only simple known names and confirms the
//! original is absent. Conditional failures, complex syntax, and operator traps may omit hints.
//! Embedded shells do not implement these Bash hooks and are deliberately left unchanged.

pub(super) fn decorate(script: &str) -> String {
    format!("{PRELUDE}\n{script}")
}

const PRELUDE: &str = r#"if ! builtin declare -F command_not_found_handle >/dev/null && ! builtin declare -F __dial_missing_program >/dev/null && ! builtin declare -F __dial_missing_program_err >/dev/null; then
__dial_missing_program() {
    local candidate resolved
    case "$1" in
        python) candidate='python3' ;;
        python3) candidate='python' ;;
        pip) candidate='pip3' ;;
        pip3) candidate='pip' ;;
        node) candidate='nodejs' ;;
        nodejs) candidate='node' ;;
        *) return 0 ;;
    esac
    resolved=$(builtin type -P "$candidate" 2>/dev/null) || resolved=''
    if [ -f "$resolved" ] && [ -x "$resolved" ]; then
        builtin printf '[missing program: %s; available alternative in this shell PATH: %s; compatibility not checked]\n' "$1" "$candidate" >&2
    else
        builtin printf '[missing program: %s; checked alternative: %s; not available in this shell PATH]\n' "$1" "$candidate" >&2
    fi
    return 0
}
if (( BASH_VERSINFO[0] >= 4 )); then
command_not_found_handle() {
    builtin printf '%s: command not found\n' "$1" >&2
    __dial_missing_program "$1"
    return 127
}
elif [ -z "$(builtin trap -p ERR)" ]; then
__dial_missing_program_err() {
    [ "$1" = 127 ] || return 0
    local program
    case "$2" in
        python|python\ *) program='python' ;;
        python3|python3\ *) program='python3' ;;
        pip|pip\ *) program='pip' ;;
        pip3|pip3\ *) program='pip3' ;;
        node|node\ *) program='node' ;;
        nodejs|nodejs\ *) program='nodejs' ;;
        *) return 0 ;;
    esac
    if ! builtin type -t "$program" >/dev/null 2>&1; then
        __dial_missing_program "$program"
    fi
    return 0
}
builtin trap '__dial_missing_program_err "$?" "$BASH_COMMAND"' ERR
fi
fi"#;

#[cfg(all(test, unix))]
mod tests;
