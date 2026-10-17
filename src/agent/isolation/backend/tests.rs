use crate::config::isolation::IsolationConfig;
use crate::config::isolation::Preference;

use super::*;

fn all_ok(_: Backend) -> Result<(), String> {
    Ok(())
}

#[test]
fn auto_order_is_platform_specific() {
    assert_eq!(
        auto_order_for("linux"),
        &[Backend::Overlay, Backend::Worktree, Backend::Copy]
    );
    assert_eq!(
        auto_order_for("windows"),
        &[Backend::ProjFs, Backend::Worktree, Backend::Copy]
    );
    assert_eq!(
        auto_order_for("macos"),
        &[Backend::Clone, Backend::Worktree, Backend::Copy]
    );
    assert_eq!(auto_order_for("freebsd"), &[Backend::Copy]);
}

#[test]
fn auto_takes_the_first_available_candidate_in_order() {
    let order = [Backend::Clone, Backend::Worktree, Backend::Copy];
    let selection = select_from(Preference::Auto, &order, all_ok).unwrap();
    assert_eq!(selection.backend, Backend::Clone);
    assert!(selection.skipped.is_empty());
    assert_eq!(selection.label(), "clone");
}

#[test]
fn auto_records_each_skipped_candidate_with_its_reason() {
    let order = [Backend::Clone, Backend::Worktree, Backend::Copy];
    let selection = select_from(Preference::Auto, &order, |backend| match backend {
        Backend::Clone => Err("no clonefile".into()),
        Backend::Worktree => Err("not a git repository".into()),
        _ => Ok(()),
    })
    .unwrap();
    assert_eq!(selection.backend, Backend::Copy);
    assert_eq!(
        selection.skipped,
        vec![
            (Backend::Clone, "no clonefile".to_owned()),
            (Backend::Worktree, "not a git repository".to_owned()),
        ]
    );
    assert_eq!(
        selection.label(),
        "copy; clone skipped (no clonefile); worktree skipped (not a git repository)"
    );
}

#[test]
fn auto_never_picks_a_backend_outside_the_platform_order() {
    let order = auto_order_for(std::env::consts::OS);
    let selection = select(Preference::Auto, |backend| match backend {
        Backend::Clone => Err("unavailable".into()),
        _ => Ok(()),
    })
    .unwrap();
    assert!(order.contains(&selection.backend), "{selection:?}");
    assert_ne!(selection.backend, Backend::Clone);
    assert_eq!(
        selection.backend,
        order
            .iter()
            .copied()
            .find(|b| *b != Backend::Clone)
            .unwrap()
    );
}

#[test]
fn auto_errors_when_every_candidate_is_unavailable() {
    let order = [Backend::Overlay, Backend::Copy];
    let error = select_from(Preference::Auto, &order, |_| Err("nope".into())).unwrap_err();
    assert!(error.contains("no isolation backend"), "{error}");
}

#[test]
fn named_backend_is_used_without_consulting_auto_order() {
    let selection = select_from(Preference::Worktree, &[], all_ok).unwrap();
    assert_eq!(selection.backend, Backend::Worktree);
    assert!(selection.skipped.is_empty());
}

#[test]
fn named_backend_that_is_unavailable_errors_instead_of_falling_back() {
    let error = select_from(
        Preference::Overlay,
        &[Backend::Copy],
        |backend| match backend {
            Backend::Overlay => Err("needs overlayfs".into()),
            _ => Ok(()),
        },
    )
    .unwrap_err();
    assert!(
        error.contains("`overlay` is unavailable: needs overlayfs"),
        "{error}"
    );
}

#[test]
fn probe_results_are_computed_once_per_backend_project_and_budget() {
    let project = PathBuf::from("/dial-test/probe-cache-project");
    let mut calls = 0;
    let first = cached(Backend::ProjFs, &project, 10, || {
        calls += 1;
        Err("no projfs".into())
    });
    let second = cached(Backend::ProjFs, &project, 10, || {
        calls += 1;
        Ok(())
    });
    assert_eq!(calls, 1, "the second lookup reuses the first answer");
    assert_eq!(first, Err("no projfs".into()));
    assert_eq!(second, Err("no projfs".into()));

    // A different budget is a different scan, so it is computed again.
    assert!(cached(Backend::ProjFs, &project, 11, || Ok(())).is_ok());

    let other = PathBuf::from("/dial-test/probe-cache-other");
    assert!(cached(Backend::ProjFs, &other, 10, || Ok(())).is_ok());
}

#[test]
fn preference_parses_lowercase_names_and_rejects_unknown_ones() {
    let parsed: IsolationConfig = toml::from_str("backend = \"worktree\"").unwrap();
    assert_eq!(parsed.backend, Preference::Worktree);
    let projfs: IsolationConfig = toml::from_str("backend = \"projfs\"").unwrap();
    assert_eq!(projfs.backend, Preference::ProjFs);
    assert!(toml::from_str::<IsolationConfig>("backend = \"bogus\"").is_err());
    assert!(toml::from_str::<IsolationConfig>("bogus = true").is_err());
}
