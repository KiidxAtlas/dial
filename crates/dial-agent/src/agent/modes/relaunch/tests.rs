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
