use super::*;

#[test]
fn repeated_batches_reuse_bounded_lanes_and_keep_input_order() {
    with_workers(
        3,
        |item| (item, std::thread::current().id()),
        |prepare| {
            let expected = [9, 8, 7, 6, 5, 4, 3];
            let first = prepare(&expected);
            assert_eq!(first.iter().map(|r| r.0).collect::<Vec<_>>(), expected);
            let threads = |batch: &[(i32, std::thread::ThreadId)]| {
                batch
                    .iter()
                    .map(|r| r.1)
                    .collect::<std::collections::HashSet<_>>()
            };
            let started = threads(&first);
            assert_eq!(started.len(), 3);
            assert_eq!(threads(&prepare(&[4, 3, 2])), started);
        },
    );
}
