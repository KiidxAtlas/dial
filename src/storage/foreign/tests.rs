use super::*;

#[test]
fn tables_have_no_duplicate_directories() {
    for table in [PROJECT_SKILL_ROOTS, USER_SKILL_ROOTS] {
        let mut rels: Vec<&str> = table.iter().map(|r| r.rel).collect();
        rels.sort_unstable();
        let before = rels.len();
        rels.dedup();
        assert_eq!(before, rels.len());
    }
}

#[test]
fn entries_are_relative_and_confined() {
    for r in PROJECT_SKILL_ROOTS.iter().chain(USER_SKILL_ROOTS) {
        assert!(
            !r.rel.starts_with('/') && !r.rel.contains(".."),
            "{}",
            r.rel
        );
    }
}
