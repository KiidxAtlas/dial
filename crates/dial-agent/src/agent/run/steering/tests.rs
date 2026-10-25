use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use tungstenite::{Message, WebSocket};

fn receive(socket: &mut WebSocket<TcpStream>) -> Value {
    loop {
        if let Message::Text(text) = socket.read().unwrap() {
            return serde_json::from_str(text.as_ref()).unwrap();
        }
    }
}

fn send(socket: &mut WebSocket<TcpStream>, event: Value) {
    socket
        .send(Message::Text(event.to_string().into()))
        .unwrap();
}

/// Real transport receipts: the server accepts the first input and deliberately leaves the
/// second outstanding. Channels establish receipt processing without a scheduling-dependent sleep.
struct Receipts {
    key: String,
    interrupt: Arc<AtomicBool>,
    release: mpsc::Sender<()>,
    server: Option<thread::JoinHandle<()>>,
    client: Option<thread::JoinHandle<()>>,
}

impl Receipts {
    fn start() -> (Self, LiveSteering) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let key = format!("agent-settlement-{address}");
        let (release, released) = mpsc::channel();
        let (submitted, submission) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            assert_eq!(receive(&mut socket)["type"], "response.create");
            send(
                &mut socket,
                json!({"type":"response.created","response":{"id":"running"}}),
            );
            send(
                &mut socket,
                json!({"type":"response.output_text.delta","delta":"started"}),
            );
            assert_eq!(receive(&mut socket)["type"], "response.steer");
            send(
                &mut socket,
                json!({"type":"response.steer.accepted","steer":{"id":"accepted","previous_response_id":"running"}}),
            );
            send(
                &mut socket,
                json!({"type":"response.output_text.delta","delta":"accepted"}),
            );
            assert_eq!(receive(&mut socket)["type"], "response.steer");
            submitted.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(15));
        });
        let interrupt = Arc::new(AtomicBool::new(false));
        let client_interrupt = interrupt.clone();
        let client_key = key.clone();
        let (progress, progressed) = mpsc::channel();
        let client = thread::spawn(move || {
            let url = format!("http://{address}/codex/responses");
            let mut on_text = |_: &str| {
                let _ = progress.send(());
                true
            };
            let _ = llm::fake::codex_exchange(&url, &client_key, client_interrupt, &mut on_text);
        });
        let fixture = Self {
            key,
            interrupt,
            release,
            server: Some(server),
            client: Some(client),
        };
        progressed.recv_timeout(Duration::from_secs(5)).unwrap();
        let first = fixture.submit("accepted prefix");
        progressed.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = fixture.submit("uncertain suffix");
        submission.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(second.ticket.as_ref().unwrap().try_outcome().is_none());
        let live = LiveSteering {
            pending: [first, second].into(),
            replanning: false,
        };
        (fixture, live)
    }

    fn submit(&self, text: &str) -> PendingSteer {
        let message = json!({"role":"user","content":text,"dial_context_request":true});
        let input = llm::responses::input_items(std::slice::from_ref(&message)).unwrap();
        let ticket = llm::codex_ws::submit(&self.key, input)
            .unwrap()
            .expect("active loopback response");
        PendingSteer {
            message,
            text: text.into(),
            ticket: Some(ticket),
            deadline: Instant::now() + LIVE_STEER_ACK,
        }
    }
}

impl Drop for Receipts {
    fn drop(&mut self) {
        self.interrupt.store(true, Ordering::SeqCst);
        let _ = self.release.send(());
        self.server.take().unwrap().join().unwrap();
        self.client.take().unwrap().join().unwrap();
        llm::codex_ws::forget(&self.key);
    }
}

struct Input {
    interrupt: Option<Arc<AtomicBool>>,
    new_input: Option<String>,
    pending: Vec<String>,
    applied: Vec<String>,
    beats: usize,
}

impl Ui for Input {
    fn heartbeat(&mut self) {
        self.beats += 1;
        if self.beats == 2 {
            if let Some(interrupt) = &self.interrupt {
                interrupt.store(true, Ordering::SeqCst);
            }
            self.pending.extend(self.new_input.take());
        }
    }
    fn take_steering(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending)
    }
    fn steering_applied(&mut self, text: &str) {
        self.applied.push(text.into());
    }
}

#[test]
fn outstanding_receipt_settlement_services_abort_and_preserves_order_once() {
    let (_receipts, mut live) = Receipts::start();
    with_agent("", |agent| {
        let mut ui = Input {
            interrupt: Some(agent.interrupt.clone()),
            new_input: None,
            pending: vec![],
            applied: vec![],
            beats: 0,
        };
        settle(
            agent,
            &mut live,
            &mut ui,
            &["accepted prefix", "uncertain suffix"],
        );
        assert!(agent.interrupt.load(Ordering::SeqCst));
    });
}

#[test]
fn outstanding_receipt_settlement_services_new_input_after_accepted_prefix_and_uncertain_suffix() {
    let (_receipts, mut live) = Receipts::start();
    with_agent("", |agent| {
        let mut ui = Input {
            interrupt: None,
            new_input: Some("newer instruction".into()),
            pending: vec![],
            applied: vec![],
            beats: 0,
        };
        settle(
            agent,
            &mut live,
            &mut ui,
            &["accepted prefix", "uncertain suffix", "newer instruction"],
        );
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            "newer instruction"
        );
    });
}

fn settle(agent: &mut Agent<'_>, live: &mut LiveSteering, ui: &mut Input, expected: &[&str]) {
    assert!(!agent.poll_live_steering(ui, live).unwrap());
    assert_eq!(agent.accepted_live_steering.len(), 1);
    assert_eq!(live.pending.len(), 1);
    let started = Instant::now();
    assert!(agent.settle_live_steering(ui, live).unwrap());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "settlement waited for receipt deadline"
    );
    assert!(ui.beats >= 2);
    assert!(live.replanning);
    assert!(live.pending.is_empty());
    assert_eq!(ui.applied, expected);
    assert!(!agent.flush_accepted_live_steering(ui).unwrap());
    assert!(!agent.take_steering(ui).unwrap());
    let mut messages = vec![];
    agent.session.extend_messages(&mut messages);
    let users: Vec<_> = messages
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert_eq!(
        users, expected,
        "journal accepted prefix, uncertain suffix and later input exactly once in submission order"
    );
}
