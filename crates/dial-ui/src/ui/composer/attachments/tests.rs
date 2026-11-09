use super::super::{ComposerEvent, SteerKind};
use super::*;

fn preview() -> Preview {
    Preview {
        summary: "0 included · 1 skipped".into(),
        issues: vec!["missing file".into()],
        items: vec![PreviewItem {
            reference: "missing".into(),
            detail: "skipped".into(),
        }],
    }
}

fn ready() -> Composer {
    let mut composer = Composer::new(Vec::new(), 10);
    composer.set_draft("explain @missing");
    composer.refresh_attachments();
    composer.attachments.result = Some(preview());
    composer
}

#[test]
fn material_issues_preserve_draft_for_every_send_route() {
    let routes: [fn(&mut Composer) -> ComposerEvent; 3] = [
        Composer::submit,
        |c| c.submit_queued(true),
        |c| c.send_explicit(SteerKind::Steer),
    ];
    for route in routes {
        let mut composer = ready();
        assert_eq!(route(&mut composer), ComposerEvent::None);
        assert_eq!(composer.draft(), "explain @missing");
        assert!(composer.history().is_empty());
        assert!(composer.queued_messages().is_empty());
        assert!(composer.explicit.is_empty());
        route(&mut composer);
        assert!(composer.draft().is_empty());
        assert_eq!(composer.history(), vec!["explain @missing"]);
    }
}

#[test]
fn changed_draft_and_provider_clear_confirmation_and_stale_metadata() {
    let mut composer = ready();
    assert!(!composer.attachment_send_allowed());
    composer.set_draft("different @missing");
    assert!(!composer.attachments.armed);
    assert!(composer.attachments.result.is_none());
    composer.attachments.result = Some(preview());
    composer.attachments.armed = true;
    composer.set_attachment_provider(None);
    assert!(!composer.attachments.armed);
    assert!(composer.attachments.result.is_none());
}

fn wait_finished(state: &Preflight) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state.running.as_ref().unwrap().handle.is_finished() {
        assert!(
            Instant::now() < deadline,
            "attachment worker did not finish"
        );
        std::thread::yield_now();
    }
}

fn completed_worker(generation: u64, result: Option<Preview>) -> Running {
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        if let Some(result) = result {
            tx.send(result).unwrap();
        }
    });
    Running {
        generation,
        rx,
        handle,
    }
}

#[test]
fn pending_preview_never_sends_or_arms_confirmation() {
    let mut composer = ready();
    composer.attachments.result = None;
    let (release, wait) = mpsc::channel();
    let (_, rx) = mpsc::channel();
    composer.attachments.running = Some(Running {
        generation: composer.attachments.generation,
        rx,
        handle: std::thread::spawn(move || {
            wait.recv().unwrap();
        }),
    });
    assert_eq!(composer.submit(), ComposerEvent::None);
    assert_eq!(composer.draft(), "explain @missing");
    assert!(!composer.attachments.armed);
    release.send(()).unwrap();
    wait_finished(&composer.attachments);
    composer.poll_attachments();
}

#[test]
fn superseded_worker_result_cannot_replace_latest_preview() {
    let mut state = Preflight::default();
    state.generation = 2;
    state.running = Some(completed_worker(1, Some(preview())));
    wait_finished(&state);
    assert!(!state.poll());
    assert!(state.running.is_none());
    assert!(state.result.is_none());
    state.running = Some(completed_worker(2, Some(preview())));
    wait_finished(&state);
    assert!(state.poll());
    assert_eq!(state.result.as_ref().unwrap().issues, vec!["missing file"]);
}

#[test]
fn removal_uses_injected_canonical_adapter_and_only_edits_draft() {
    let mut composer = ready();
    composer.attachments.provider = Some(Provider {
        candidates: Arc::new(|_| false),
        preview: Arc::new(|_, _| Preview::default()),
        remove: Arc::new(|text, reference| {
            assert_eq!(text, "explain @missing");
            assert_eq!(reference, "missing");
            Some("explain ".into())
        }),
    });
    assert!(composer.remove_attachment(0));
    assert_eq!(composer.draft(), "explain ");
    assert!(composer.history().is_empty());
}

#[test]
fn slow_preview_coalesces_edits_without_spawning_another_worker() {
    let (started_tx, started_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let finish_rx = std::sync::Mutex::new(finish_rx);
    let mut state = Preflight::default();
    state.provider = Some(Provider {
        candidates: Arc::new(|_| true),
        preview: Arc::new(move |text, _| {
            started_tx.send(text.to_owned()).unwrap();
            finish_rx.lock().unwrap().recv().unwrap();
            Preview {
                summary: text.into(),
                ..Preview::default()
            }
        }),
        remove: Arc::new(|_, _| None),
    });
    state.update("@one".into());
    state.pending = Some(Instant::now() - Duration::from_secs(1));
    state.poll();
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "@one"
    );
    state.update("@two".into());
    state.update("@three".into());
    assert!(state.cancel.load(Ordering::Relaxed));
    state.pending = Some(Instant::now() - Duration::from_secs(1));
    state.poll();
    assert!(started_rx.try_recv().is_err());
    finish_tx.send(()).unwrap();
    wait_finished(&state);
    state.poll();
    assert!(state.result.is_none());
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "@three"
    );
    finish_tx.send(()).unwrap();
    wait_finished(&state);
    state.poll();
    assert_eq!(state.result.as_ref().unwrap().summary, "@three");
}

#[test]
fn disconnected_worker_reports_unverified_input_and_requires_confirmation() {
    let mut composer = ready();
    composer.attachments.result = None;
    composer.attachments.running = Some(completed_worker(composer.attachments.generation, None));
    wait_finished(&composer.attachments);
    assert!(!composer.attachment_send_allowed());
    assert!(!composer.attachments.pending());
    let result = composer.attachments.result.as_ref().unwrap();
    assert!(result.items.is_empty());
    assert!(result.summary.contains("no inclusion verified"));
    assert!(result.issues[0].contains("without a result"));
    assert!(composer.attachment_send_allowed());
}

#[test]
fn panicking_provider_is_collected_without_poisoning_later_generations() {
    let mut composer = ready();
    composer.set_attachment_provider(Some(Provider {
        candidates: Arc::new(|_| true),
        preview: Arc::new(|_, _| panic!("test attachment provider failure")),
        remove: Arc::new(|_, _| None),
    }));
    composer.attachments.pending = Some(Instant::now() - Duration::from_secs(1));
    composer.poll_attachments();
    wait_finished(&composer.attachments);
    assert!(!composer.attachment_send_allowed());
    assert!(!composer.attachments.pending());
    assert!(composer.attachments.result.as_ref().unwrap().issues[0].contains("panicked"));
    assert!(composer.attachment_send_allowed());

    // Keep the old worker in flight through set_draft, which can poll and reap it.
    let (release, wait) = mpsc::channel();
    composer.attachments.running = Some(Running {
        generation: composer.attachments.generation,
        rx: mpsc::channel().1,
        handle: std::thread::spawn(move || {
            wait.recv().unwrap();
            panic!("stale worker");
        }),
    });
    composer.set_draft("new @draft");
    release.send(()).unwrap();
    wait_finished(&composer.attachments);
    composer.poll_attachments();
    assert!(composer.attachments.result.is_none());
    assert!(!composer.attachments.armed);
}

#[test]
fn worker_start_failure_is_an_explicit_issue_and_stale_failures_are_ignored() {
    let mut composer = ready();
    let generation = composer.attachments.generation;
    assert!(
        !composer
            .attachments
            .failed(generation.wrapping_add(1), "could not start worker")
    );
    assert_eq!(
        composer.attachments.result.as_ref().unwrap().issues,
        vec!["missing file"]
    );
    assert!(
        composer
            .attachments
            .failed(generation, "could not start worker")
    );
    assert!(!composer.attachment_send_allowed());
    assert!(!composer.attachments.pending());
    assert!(
        composer.attachments.result.as_ref().unwrap().issues[0].contains("could not start worker")
    );
    assert!(composer.attachment_send_allowed());
}
