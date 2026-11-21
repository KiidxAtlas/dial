use super::*;

#[test]
fn budget_expiry_is_reported_as_a_timeout_before_the_work_error() {
    let cancel = Arc::new(AtomicBool::new(false));
    let started = std::time::Instant::now();
    let error = within_budget(&cancel, Duration::from_millis(20), |flag| -> Result<()> {
        while !flag.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        anyhow::bail!("work error")
    })
    .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(
        cancel.load(Ordering::SeqCst),
        "the expired budget also sets the shared cancel flag"
    );
}
