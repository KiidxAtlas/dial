use super::*;

#[test]
fn every_command_has_a_unique_name_and_synopsis() {
    let mut names: Vec<&str> = UTILS.iter().map(|u| u.name).collect();
    names.sort_unstable();
    let unique = names.len();
    names.dedup();
    assert_eq!(names.len(), unique);
    assert!(UTILS.iter().all(|u| u.synopsis.starts_with(u.name)));
}

#[test]
fn registrations_cover_every_command() {
    assert_eq!(registrations().len(), UTILS.len());
}
