use super::*;

#[test]
fn starter_lists_are_everyday_ordered_and_unique() {
    for list in [START_HERE, STARTERS] {
        let mut seen = std::collections::HashSet::new();
        for name in list {
            assert!(everyday(name), "/{name} starts a list but is not everyday");
            assert!(seen.insert(name), "/{name} listed twice");
        }
    }
}
