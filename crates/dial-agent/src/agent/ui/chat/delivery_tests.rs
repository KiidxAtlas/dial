use super::*;

#[test]
fn fast_provider_preserves_live_text_and_drops_all_deltas_after_cancel() {
    for mode in 0..3 {
        let cancel = mode == 1;
        struct View {
            interrupt: Arc<AtomicBool>,
            cancel: bool,
            fail: bool,
            text: String,
            reasoning: String,
        }
        impl Ui for View {
            fn execution_error(&self) -> Option<String> {
                (self.fail && !self.text.is_empty()).then(|| "journal stopped".into())
            }
            fn text_delta(&mut self, text: &str) {
                self.text.push_str(text);
                if self.cancel {
                    self.interrupt.store(true, Ordering::SeqCst);
                }
            }
            fn reasoning_delta(&mut self, text: &str) {
                self.reasoning.push_str(text);
            }
        }
        let mut wire = String::new();
        for i in 0..64 {
            wire.push_str(&format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{"content":format!("chunk-{i};")}}]})
            ));
            wire.push_str(&format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{"reasoning_content":"thinking;"}}]})
            ));
        }
        wire.push_str(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        let server = llm::fake::serve(vec![llm::fake::Script::Sse(wire)]);
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: format!("{}/v1", server.url),
                model: "test".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        );
        let interrupt = Arc::new(AtomicBool::new(false));
        let mut view = View {
            interrupt: interrupt.clone(),
            cancel,
            fail: mode == 2,
            text: String::new(),
            reasoning: String::new(),
        };
        let result = chat_attempt_with_ui(
            &mut backend,
            None,
            &Chat {
                messages: vec![json!({"role":"user","content":"hello"})],
                tools: vec![],
                temperature: 0.0,
                max_tokens: 1024,
                seed: None,
            },
            ChatOptions {
                timeout: 5,
                render_text: true,
                context_tokens: None,
                policy: None,
                recovery: None,
            },
            interrupt,
            ChatSinks {
                ui: &mut view,
                on_idle: &mut |_| {},
            },
            ChatMonitors {
                rules: None,
                stream: None,
            },
        );
        if cancel {
            assert!(
                result
                    .unwrap_err()
                    .downcast_ref::<llm::Interrupted>()
                    .is_some()
            );
            assert_eq!(view.text, "chunk-0;");
            assert!(
                view.reasoning.is_empty(),
                "queued reasoning escaped cancellation"
            );
        } else if mode == 2 {
            assert_eq!(result.unwrap_err().to_string(), "journal stopped");
            assert_eq!(view.text, "chunk-0;");
            assert!(view.reasoning.is_empty());
        } else {
            result.unwrap();
            assert_eq!(
                view.text,
                (0..64).map(|i| format!("chunk-{i};")).collect::<String>()
            );
            assert_eq!(view.reasoning, "thinking;".repeat(64));
        }
        assert_eq!(server.requests().len(), 1);
    }
}
