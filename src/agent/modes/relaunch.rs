//! `/restart` relaunch planning. The process is replaced with the same launch flags, resuming the
//! current session when it is persisted on disk. The exec itself must run after the terminal has
//! been restored, so the caller (app exit hook) invokes [`exec`] once the TUI is gone.
use std::ffi::OsString;

/// Flags that select a session at launch. They are dropped so the planned `--resume` is the only
/// selector (`-c` / `--continue` conflict with `--resume`).
const SESSION_FLAGS: &[&str] = &["-c", "--continue", "-r", "--resume", "-s", "--session"];

/// One-shot launch flags. Replaying them into a resumed session would arm them again (a second goal,
/// a second plan-yolo approval), so the relaunch drops them.
const ONE_SHOT_VALUE_FLAGS: &[&str] = &["--goal", "--plan-yolo-into"];
const ONE_SHOT_FLAGS: &[&str] = &["--plan-yolo"];

/// Argv (excluding the program name) for the relaunched process.
///
/// `launch` is the original argv minus the program name. `resume_id` is the current session id,
/// passed only when its session file exists on disk (an unsaved session has nothing to resume).
pub fn relaunch_args(launch: &[OsString], resume_id: Option<&str>) -> Vec<OsString> {
    let mut out = Vec::with_capacity(launch.len() + 2);
    let mut skip_value = false;
    let mut skip_one_shot_value = false;
    for arg in launch {
        if skip_one_shot_value {
            skip_one_shot_value = false;
            continue;
        }
        if skip_value {
            // A bare `--resume <id>` or `-r <id>` consumed the next token only when it is a value,
            // so a following flag ends the skip.
            skip_value = arg.to_str().is_some_and(|s| !s.starts_with('-'));
            if skip_value {
                skip_value = false;
                continue;
            }
        }
        let text = arg.to_string_lossy();
        // `-rVALUE` carries its value inline (clap short-flag form); the planned `--resume` replaces it.
        if text.len() > 2 && text.starts_with("-r") {
            continue;
        }
        if let Some((flag, _)) = text.split_once('=')
            && SESSION_FLAGS.contains(&flag)
        {
            continue;
        }
        if SESSION_FLAGS.contains(&text.as_ref()) {
            // `--resume`, `-r` and `--session` take an optional value; `-c` / `--continue` take none.
            skip_value = !matches!(text.as_ref(), "-c" | "--continue");
            continue;
        }
        if let Some((flag, _)) = text.split_once('=')
            && ONE_SHOT_VALUE_FLAGS.contains(&flag)
        {
            continue;
        }
        if ONE_SHOT_VALUE_FLAGS.contains(&text.as_ref()) {
            skip_one_shot_value = true;
            continue;
        }
        if ONE_SHOT_FLAGS.contains(&text.as_ref()) {
            continue;
        }
        out.push(arg.clone());
    }
    if let Some(id) = resume_id {
        out.push("--resume".into());
        out.push(id.into());
    }
    out
}

/// Removes the positional prompt from `launch`. `prompt` is the words joined with single spaces
/// (`cli.prompt.join(" ")`), so the first contiguous run of argv tokens (from the end) that joins to it
/// is the prompt. The resumed session must not receive the prompt again.
pub fn without_prompt(launch: &[OsString], prompt: &str) -> Vec<OsString> {
    if prompt.is_empty() {
        return launch.to_vec();
    }
    let words: Vec<String> = launch
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    for start in (0..words.len()).rev() {
        let mut joined = String::new();
        for end in start..words.len() {
            if end > start {
                joined.push(' ');
            }
            joined.push_str(&words[end]);
            if joined == prompt {
                return launch[..start]
                    .iter()
                    .chain(&launch[end + 1..])
                    .cloned()
                    .collect();
            }
            if joined.len() >= prompt.len() {
                break;
            }
        }
    }
    launch.to_vec()
}

/// Replace the current process with `dial` relaunched with `args`. Returns only on failure.
#[cfg(unix)]
pub fn exec(args: &[OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let program = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => return err,
    };
    std::process::Command::new(program).args(args).exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn keeps_launch_flags_and_resumes_the_current_session() {
        let launch = os(&["--model", "fast", "--no-session"]);
        assert_eq!(
            relaunch_args(&launch, Some("abc123")),
            os(&["--model", "fast", "--no-session", "--resume", "abc123"])
        );
    }

    #[test]
    fn omits_resume_when_the_session_is_not_on_disk() {
        let launch = os(&["--model", "fast"]);
        assert_eq!(relaunch_args(&launch, None), os(&["--model", "fast"]));
    }

    #[test]
    fn drops_existing_session_selectors_so_the_relaunch_does_not_conflict() {
        let launch = os(&["-c", "--model", "fast"]);
        assert_eq!(
            relaunch_args(&launch, Some("x")),
            os(&["--model", "fast", "--resume", "x"])
        );

        let launch = os(&["--resume", "old", "--print"]);
        assert_eq!(
            relaunch_args(&launch, Some("new")),
            os(&["--print", "--resume", "new"])
        );

        let launch = os(&["-r", "--print"]);
        assert_eq!(relaunch_args(&launch, None), os(&["--print"]));

        let launch = os(&["--session=old"]);
        assert_eq!(
            relaunch_args(&launch, Some("new")),
            os(&["--resume", "new"])
        );
    }

    #[test]
    fn drops_attached_resume_values() {
        let launch = os(&["-rabc", "--print"]);
        assert_eq!(
            relaunch_args(&launch, Some("new")),
            os(&["--print", "--resume", "new"])
        );
    }
}
