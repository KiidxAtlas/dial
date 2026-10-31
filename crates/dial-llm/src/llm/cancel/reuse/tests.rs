use super::super::{Patience, agent, patient_agent};
use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// What the keep-alive server answers to the request with this index (counted across connections).
#[derive(Clone, Copy)]
enum Answer {
    Ok,
    /// A head promising a body that never arrives.
    Stall,
    /// A chunked event stream whose terminating chunk follows the final event a moment later.
    Sse,
}

struct Server {
    url: String,
    connections: Arc<AtomicUsize>,
}

/// A loopback HTTP/1.1 server that keeps connections open between requests.
fn keep_alive_server(answers: Vec<Answer>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::clone(&connections);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            accepted.fetch_add(1, Ordering::SeqCst);
            let answers = answers.clone();
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || serve(stream, &answers, &requests));
        }
    });
    Server { url, connections }
}

fn serve(mut stream: TcpStream, answers: &[Answer], requests: &AtomicUsize) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
        if !head.ends_with(b"\r\n\r\n") {
            continue;
        }
        head.clear();
        let index = requests.fetch_add(1, Ordering::SeqCst);
        match answers.get(index).copied().unwrap_or(Answer::Ok) {
            Answer::Ok => {
                if stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .is_err()
                {
                    return;
                }
            }
            Answer::Stall => {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n");
                std::thread::sleep(Duration::from_secs(5));
                return;
            }
            Answer::Sse => {
                let event = b"data: [DONE]\n\n";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                    event.len()
                );
                if stream.write_all(head.as_bytes()).is_err()
                    || stream.write_all(event).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
                if stream.write_all(b"0\r\n\r\n").is_err() {
                    return;
                }
            }
        }
    }
}

fn config() -> ureq::config::Config {
    ureq::Agent::config_builder()
        .proxy(None)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
}

fn get(agent: &ureq::Agent, url: &str) -> String {
    agent
        .get(url)
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap()
}

fn share_loopback() {
    SHARE_LOOPBACK.with(|share| share.set(true));
}

#[test]
fn requests_from_separate_agents_share_one_finished_connection() {
    share_loopback();
    let server = keep_alive_server(vec![]);
    for _ in 0..3 {
        // A fresh agent per request, as every model request builds one.
        let agent = agent(config(), Arc::new(AtomicBool::new(false)));
        assert_eq!(get(&agent, &server.url), "ok");
    }
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

#[test]
fn a_stream_read_to_its_final_event_returns_its_connection() {
    share_loopback();
    let server = keep_alive_server(vec![Answer::Sse]);
    let patience = Patience::new(Some(Duration::from_secs(5)), Duration::from_secs(5));
    let first = patient_agent(config(), Arc::new(AtomicBool::new(false)), patience.clone());
    let mut response = first.get(&server.url).call().unwrap();
    let mut events = Vec::new();
    crate::llm::stream::read_events(
        response.body_mut().as_reader(),
        1 << 20,
        1 << 16,
        patience,
        |data| {
            events.push(data.to_owned());
            Ok(data == "[DONE]")
        },
    )
    .unwrap();
    assert_eq!(events, ["[DONE]"]);
    drop(response);
    drop(first);
    let second = agent(config(), Arc::new(AtomicBool::new(false)));
    assert_eq!(get(&second, &server.url), "ok");
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

#[test]
fn a_borrowed_connection_obeys_the_new_requests_interrupt_and_limits() {
    share_loopback();
    let server = keep_alive_server(vec![Answer::Ok, Answer::Stall]);
    let first = agent(config(), Arc::new(AtomicBool::new(false)));
    assert_eq!(get(&first, &server.url), "ok");
    drop(first);

    let interrupt = Arc::new(AtomicBool::new(false));
    let patience = Patience::new(Some(Duration::from_secs(30)), Duration::from_secs(30));
    let second = patient_agent(config(), Arc::clone(&interrupt), patience);
    let mut response = second.get(&server.url).call().unwrap();
    let trigger = std::thread::spawn({
        let interrupt = Arc::clone(&interrupt);
        move || {
            std::thread::sleep(Duration::from_millis(100));
            interrupt.store(true, Ordering::SeqCst);
        }
    });
    let started = Instant::now();
    let error = response
        .body_mut()
        .as_reader()
        .read_to_end(&mut Vec::new())
        .unwrap_err();
    trigger.join().unwrap();
    assert!(error.to_string().contains("interrupted"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        1,
        "the stalled request ran on the borrowed connection"
    );
}

#[test]
fn an_unfinished_reply_never_returns_its_connection() {
    share_loopback();
    let server = keep_alive_server(vec![]);
    let first = agent(config(), Arc::new(AtomicBool::new(false)));
    let response = first.get(&server.url).call().unwrap();
    // The body is never read: the connection may still carry bytes of this reply.
    drop(response);
    drop(first);
    let second = agent(config(), Arc::new(AtomicBool::new(false)));
    assert_eq!(get(&second, &server.url), "ok");
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}

#[test]
fn loopback_hosts_are_not_shared_by_default() {
    let server = keep_alive_server(vec![]);
    for _ in 0..2 {
        let agent = agent(config(), Arc::new(AtomicBool::new(false)));
        assert_eq!(get(&agent, &server.url), "ok");
    }
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}
