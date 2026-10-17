use super::*;

fn config() -> ImproveConfig {
    ImproveConfig {
        daily_compute_minutes: 120,
        ..ImproveConfig::default()
    }
}

#[test]
fn each_condition_blocks_and_a_free_machine_does_not() {
    let c = config();
    assert_eq!(blocker(&c, &Snapshot::permissive()), None);
    let free = Snapshot {
        on_ac: Some(true),
        idle_secs: Some(3600),
        ..Snapshot::permissive()
    };
    assert_eq!(blocker(&c, &free), None);
    assert!(
        blocker(
            &c,
            &Snapshot {
                on_ac: Some(false),
                ..free.clone()
            }
        )
        .unwrap()
        .contains("battery")
    );
    assert!(
        blocker(
            &c,
            &Snapshot {
                low_power: true,
                ..free.clone()
            }
        )
        .unwrap()
        .contains("Low Power")
    );
    assert!(
        blocker(
            &c,
            &Snapshot {
                throttled: true,
                ..free.clone()
            }
        )
        .unwrap()
        .contains("throttled")
    );
    assert!(
        blocker(
            &c,
            &Snapshot {
                idle_secs: Some(60),
                ..free.clone()
            }
        )
        .unwrap()
        .contains("idle")
    );
    let used = u64::from(c.daily_compute_minutes) * 60;
    assert!(
        blocker(
            &c,
            &Snapshot {
                used_today_secs: used,
                ..free.clone()
            }
        )
        .unwrap()
        .contains("budget")
    );
    let relaxed = ImproveConfig {
        require_ac_power: false,
        idle_minutes: 0,
        daily_compute_minutes: 0,
        ..c
    };
    assert_eq!(
        blocker(
            &relaxed,
            &Snapshot {
                on_ac: Some(false),
                idle_secs: Some(0),
                used_today_secs: 10_000_000,
                ..free
            }
        ),
        None
    );
}

#[test]
fn concurrent_settlements_keep_every_increment_without_compute_locks() {
    let dir = tempfile::tempdir().unwrap();
    let at = 86_400 * 100 + 5;
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                for _ in 0..25 {
                    record_usage_at(dir.path(), 3, at).unwrap();
                }
            });
        }
    });
    assert_eq!(read_usage(dir.path(), at), 8 * 25 * 3);
}

#[test]
fn corrupt_usage_record_is_reported_and_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let at = 86_400 * 100 + 5;
    let corrupt = b"{\"day\": 1e";
    std::fs::write(usage_path(dir.path()), corrupt).unwrap();
    assert!(record_usage_at(dir.path(), 60, at).is_err());
    assert_eq!(std::fs::read(usage_path(dir.path())).unwrap(), corrupt);
    let config = ImproveConfig {
        daily_compute_minutes: 120,
        ..ImproveConfig::default()
    };
    let snapshot = Snapshot {
        usage_unreadable: read_usage_checked(dir.path(), at).is_err(),
        ..Snapshot::permissive()
    };
    assert!(blocker(&config, &snapshot).unwrap().contains("unreadable"));
    // A corrupt record is not a day's usage: an unbudgeted machine is unaffected.
    let unbudgeted = ImproveConfig {
        daily_compute_minutes: 0,
        ..config
    };
    assert_eq!(blocker(&unbudgeted, &snapshot), None);
}

#[test]
fn pmset_output_is_parsed() {
    assert_eq!(
        parse_power_source("Now drawing from 'AC Power'\n -InternalBattery-0 100%"),
        Some(true)
    );
    assert_eq!(
        parse_power_source("Now drawing from 'Battery Power'"),
        Some(false)
    );
    assert_eq!(parse_power_source("garbage"), None);
    assert!(parse_low_power(
        "System-wide power settings:\n lowpowermode         1\n sleep 1"
    ));
    assert!(!parse_low_power(" lowpowermode         0"));
    assert!(parse_throttled(
        "CPU_Scheduler_Limit = 100\nCPU_Available_CPUs = 8\nCPU_Speed_Limit = 70"
    ));
    assert!(!parse_throttled("CPU_Speed_Limit = 100"));
    assert!(!parse_throttled(
        "Note: No thermal warning level has been recorded"
    ));
}

#[test]
fn usage_accumulates_per_day_and_resets_at_midnight() {
    let dir = tempfile::tempdir().unwrap();
    let monday = 86_400 * 100 + 5;
    record_usage_at(dir.path(), 600, monday).unwrap();
    record_usage_at(dir.path(), 300, monday + 10).unwrap();
    assert_eq!(read_usage(dir.path(), monday), 900);
    assert_eq!(read_usage(dir.path(), monday + 86_400), 0);
    record_usage_at(dir.path(), 60, monday + 86_400).unwrap();
    assert_eq!(read_usage(dir.path(), monday + 86_400), 60);
    assert_eq!(
        budget_left(
            &ImproveConfig {
                daily_compute_minutes: 10,
                ..config()
            },
            900
        ),
        Some(0)
    );
    assert_eq!(
        budget_left(
            &ImproveConfig {
                daily_compute_minutes: 0,
                ..config()
            },
            900
        ),
        None
    );
}

#[test]
fn phases_are_bucketed_and_timed() {
    assert_eq!(
        stage_key("evaluation check 12/60 (v3; cached results may be reused)"),
        "evaluation check"
    );
    assert_eq!(
        stage_key("scoring candidate v4 (lora)"),
        "scoring candidate v"
    );
    assert_eq!(stage_key("(x)"), "other");
    let mut timing = Timing::default();
    timing.enter("training 1/10");
    timing.enter("training 2/10");
    std::thread::sleep(std::time::Duration::from_millis(20));
    timing.enter("evaluation check 1/2");
    let report = timing.finish();
    assert!(report["training"].as_f64().unwrap() >= 0.0);
    assert!(report.get("evaluation check").is_some());
    assert_eq!(report.as_object().unwrap().len(), 2);
}

#[test]
fn foreground_marker_reports_idle_time() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(idle_secs(dir.path()), None);
    note_foreground(dir.path());
    assert!(idle_secs(dir.path()).unwrap() < 5);
}
