use super::*;
use std::time::Instant;

const DAY: u64 = CHECK_INTERVAL_SECS;

fn settle(notice: &mut ReleaseNotice, root: &Path, now: u64) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while notice.pending.is_some() {
        notice.poll_at(root, now, || unreachable!("unexpected new check"));
        assert!(Instant::now() < deadline, "background check did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn message_names_the_new_version_and_points_at_update() {
    for plan in [
        UpdatePlan::Release,
        UpdatePlan::PackageManager(release::Installation::Homebrew),
        UpdatePlan::Checkout("/src/dial".into()),
    ] {
        let text = message("v0.2.7", "0.2.6", &plan).unwrap();
        assert!(text.contains("Dial 0.2.7 is available"), "{text}");
        assert!(text.contains("you have 0.2.6"), "{text}");
        assert!(text.contains("`/update`"), "{text}");
        assert!(!text.contains("make update"), "{text}");
    }
    // Only an installation Dial cannot update itself gets other instructions.
    let nix = UpdatePlan::Manual("update the dial flake input and rebuild".into());
    let text = message("v0.2.7", "0.2.6", &nix).unwrap();
    assert!(
        text.ends_with("· update the dial flake input and rebuild"),
        "{text}"
    );
}

#[test]
fn current_or_older_releases_show_nothing() {
    let plan = UpdatePlan::Release;
    assert!(message("v0.2.6", "0.2.6", &plan).is_none());
    assert!(message("v0.2.5", "0.2.6", &plan).is_none());
    assert!(message("not-a-version", "0.2.6", &plan).is_none());
}

#[test]
fn a_found_release_is_cached_and_shown_at_once_on_the_next_launch_without_a_query() {
    let root = tempfile::tempdir().unwrap();
    let mut first = ReleaseNotice::default();
    first.poll_at(root.path(), DAY, || Ok("v999.0.0".into()));
    settle(&mut first, root.path(), DAY);
    assert!(
        first
            .notice()
            .unwrap()
            .contains("Dial 999.0.0 is available")
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join(STAMP)).unwrap(),
        format!("{DAY}\nv999.0.0\n")
    );

    let mut next = ReleaseNotice::default();
    assert!(next.poll_at(root.path(), DAY + 1, || unreachable!("checked too early")));
    assert!(next.pending.is_none());
    assert!(next.notice().unwrap().contains("999.0.0"));
}

#[test]
fn checks_run_daily_and_a_failed_query_keeps_the_known_release() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(STAMP), "10\nv999.0.0\n").unwrap();
    let mut notice = ReleaseNotice::default();
    notice.poll_at(root.path(), 10 + DAY, || anyhow::bail!("offline fixture"));
    assert!(notice.pending.is_some(), "a due check must start");
    settle(&mut notice, root.path(), 10 + DAY);
    assert!(notice.notice().unwrap().contains("999.0.0"));
    assert_eq!(
        std::fs::read_to_string(root.path().join(STAMP)).unwrap(),
        format!("{}\nv999.0.0\n", 10 + DAY),
        "the failed attempt is throttled too"
    );
}

#[test]
fn a_running_query_never_blocks_or_starts_a_second_check() {
    let root = tempfile::tempdir().unwrap();
    let (release, wait) = mpsc::channel::<()>();
    let mut notice = ReleaseNotice::default();
    let before = Instant::now();
    notice.poll_at(root.path(), DAY, move || {
        wait.recv_timeout(Duration::from_secs(3)).unwrap();
        Ok("v0.0.1".into())
    });
    notice.poll_at(root.path(), DAY * 3, || unreachable!("duplicate query"));
    assert!(before.elapsed() < Duration::from_secs(1));
    release.send(()).unwrap();
    settle(&mut notice, root.path(), DAY);
    assert!(
        notice.notice().is_none(),
        "an older release is not an update"
    );
}

#[test]
fn corrupt_or_future_stamps_do_not_suppress_checks() {
    for stamp in ["garbage", "99999999999\n"] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(STAMP), stamp).unwrap();
        let mut notice = ReleaseNotice::default();
        notice.poll_at(root.path(), DAY, || Ok("v999.0.0".into()));
        assert!(notice.pending.is_some(), "{stamp:?}");
        settle(&mut notice, root.path(), DAY);
        assert!(notice.notice().is_some(), "{stamp:?}");
    }
}
