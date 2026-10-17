use super::*;

fn score(per_example: &[f64], pair_margin: Option<f64>) -> worker::Score {
    worker::Score {
        examples: per_example.len(),
        tokens: per_example.len() * 4,
        mean_logprob: per_example.iter().sum::<f64>() / per_example.len() as f64,
        per_example: per_example.to_vec(),
        pair_examples: usize::from(pair_margin.is_some()),
        pair_margin,
        per_pair: pair_margin.into_iter().collect(),
    }
}

#[test]
fn mean_gain_needs_at_least_half_of_the_paired_examples() {
    let baseline = score(&[-1.0, -1.0, -1.0, -1.0], None);
    // A large gain on one example cannot carry a candidate that loses the other three.
    let lopsided = prescreen(&baseline, &score(&[0.0, -1.1, -1.1, -1.1], None));
    assert!(lopsided.gain() > 0.0);
    assert!(lopsided.rejection.unwrap().contains("1/4"));
    // Exactly half suffices.
    let half = prescreen(&baseline, &score(&[-0.5, -0.5, -1.1, -1.0], None));
    assert_eq!((half.wins, half.paired), (2, 4));
    assert!(half.rejection.is_none());
}

#[test]
fn ties_never_count_as_improvement() {
    let baseline = score(&[-1.0, -2.0], None);
    let same = prescreen(&baseline, &baseline.clone());
    assert_eq!(same.wins, 0);
    assert!(same.rejection.unwrap().contains("did not improve"));
    // Per-example ties are not wins, so a gain from one example of two passes only at half.
    let one = prescreen(&baseline, &score(&[-1.0, -1.5], None));
    assert_eq!(one.wins, 1);
    assert!(one.rejection.is_none());
}

#[test]
fn preference_margin_may_hold_but_not_drop() {
    let baseline = score(&[-1.0, -1.0], Some(0.8));
    let better = [-0.5, -0.5];
    assert!(
        prescreen(&baseline, &score(&better, Some(0.8)))
            .rejection
            .is_none()
    );
    let dropped = prescreen(&baseline, &score(&better, Some(0.79)));
    assert!(dropped.rejection.unwrap().contains("margin dropped"));
    assert!(
        prescreen(&baseline, &score(&better, None))
            .rejection
            .is_some()
    );
    // Without baseline pairs there is no margin to protect.
    let unpaired = score(&[-1.0, -1.0], None);
    assert!(
        prescreen(&unpaired, &score(&better, Some(-3.0)))
            .rejection
            .is_none()
    );
}

#[test]
fn mismatched_example_sets_are_rejected() {
    let baseline = score(&[-1.0, -1.0], None);
    assert!(
        prescreen(&baseline, &score(&[-0.1, -0.1, -0.1], None))
            .rejection
            .is_some()
    );
}

#[test]
fn soup_keeps_the_likeliest_and_breaks_ties_toward_the_parent() {
    assert_eq!(
        best_soup(&[(0.25, -1.2), (0.5, -1.0), (0.75, -1.1)]),
        Some(1)
    );
    assert_eq!(
        best_soup(&[(0.25, -1.0), (0.5, -1.0), (0.75, -1.5)]),
        Some(0)
    );
    assert_eq!(best_soup(&[(0.75, -1.0), (0.25, -1.0)]), Some(1));
    assert_eq!(best_soup(&[(0.25, f64::NAN), (0.5, -2.0)]), Some(1));
    assert_eq!(best_soup(&[]), None);
}
