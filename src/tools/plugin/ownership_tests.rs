use super::*;
struct Removing {
    host: std::sync::Weak<Host>,
    events: Vec<String>,
}
impl Plugin for Removing {
    fn name(&self) -> &str {
        "removing"
    }
    fn events(&self) -> &[String] {
        &self.events
    }
    fn on_event(&self, _: &str, _: &Value, _: &AtomicBool) -> anyhow::Result<Option<Value>> {
        self.host.upgrade().unwrap().unregister(self.name());
        Ok(Some(json!({"obsolete":true})))
    }
}
#[test]
fn callbacks_do_not_hold_registration_lock_and_obsolete_results_are_discarded() {
    let host = Arc::new(Host::default());
    host.register(Arc::new(Removing {
        host: Arc::downgrade(&host),
        events: vec!["test".into()],
    }))
    .unwrap();
    let owned = host.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        tx.send(owned.dispatch("test", &json!({}), &AtomicBool::new(false)))
            .unwrap();
    });
    let answers = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("registration callback deadlocked");
    worker.join().unwrap();
    assert_eq!(answers.len(), 1);
    assert!(answers[0].result.is_err());
    assert!(!host.subscribed("test"));
    host.close();
    assert!(
        host.register(Arc::new(Removing {
            host: Arc::downgrade(&host),
            events: vec!["test".into()]
        }))
        .is_err()
    );
}
