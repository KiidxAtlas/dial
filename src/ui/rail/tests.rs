use super::*;
#[test]
fn rails_fit_narrow_widths_and_keep_real_file_counts() {
    let theme = Theme::dark();
    let files = vec![FileDiff {
        path: "app/components/Widget.vue".into(),
        added: Some(12),
        removed: Some(3),
        untracked: false,
    }];
    for w in 1..150 {
        for row in session("abcdef123", "Porting UI", &files, w, 3, &theme) {
            assert!(row.width() <= w);
        }
    }
    let plain = |rows: Rows| {
        rows.into_iter()
            .flat_map(|r| r.spans.into_iter())
            .map(|s| s.content.into_owned())
            .collect::<String>()
    };
    assert!(plain(session("id", "Porting UI", &files, 100, 3, &theme)).contains("+12 −3"));
}
