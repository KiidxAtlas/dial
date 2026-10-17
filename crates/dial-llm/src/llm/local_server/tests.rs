use super::*;
use crate::llm::catalog::tests::routed;
use crate::llm::local_format::gguf_fixtures::{header, string};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

fn template_dir(template: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), "{}").unwrap();
    std::fs::write(dir.path().join("model.safetensors"), b"weights").unwrap();
    if let Some(template) = template {
        std::fs::write(dir.path().join("chat_template.jinja"), template).unwrap();
    }
    dir
}

#[test]
fn dial_runtime_reads_the_thinking_switch_from_the_mlx_template() {
    let with = template_dir(Some("{% if enable_thinking %}<think>{% endif %}"));
    let facts = facts("mlx", with.path().to_str().unwrap(), None, None, &[]);
    assert_eq!(
        facts,
        ServerFacts {
            slots: None,
            thinking_switch: Some(true)
        }
    );
    let without = template_dir(Some("{{ messages }}"));
    let facts = super::facts("mlx", without.path().to_str().unwrap(), None, None, &[]);
    assert_eq!(
        facts.thinking_switch,
        Some(false),
        "a known template with no switch"
    );
    let bare = template_dir(None);
    let facts = super::facts("mlx", bare.path().to_str().unwrap(), None, None, &[]);
    assert_eq!(facts, ServerFacts::default(), "no template is unknown");
}

#[test]
fn dial_runtime_reads_the_gguf_header_template() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model.gguf");
    let template = "{% if enable_thinking %}think{% endif %}";
    let bytes = header(&[
        ("general.architecture", string("llama")),
        ("tokenizer.chat_template", string(template)),
    ]);
    std::fs::write(&path, bytes).unwrap();
    let facts = facts("llama-cpp", path.to_str().unwrap(), None, None, &[]);
    assert_eq!(facts.thinking_switch, Some(true));
}

#[test]
fn llama_cpp_props_report_slots_and_the_template_switch() {
    let url = routed(vec![(
        "/props",
        r#"{"total_slots":4,"chat_template":"{% if enable_thinking %}x{% endif %}","default_generation_settings":{"n_ctx":4096}}"#.into(),
    )]);
    let facts = facts("llama-cpp", "qwen", Some(&url), None, &[]);
    assert_eq!(
        facts,
        ServerFacts {
            slots: Some(4),
            thinking_switch: Some(true)
        }
    );
    let plain = routed(vec![(
        "/props",
        r#"{"total_slots":1,"chat_template":"{{ messages }}","default_generation_settings":{}}"#
            .into(),
    )]);
    let facts = super::facts("llama-cpp", "qwen", Some(&plain), None, &[]);
    assert_eq!(
        facts,
        ServerFacts {
            slots: Some(1),
            thinking_switch: Some(false)
        }
    );
    let not_llama = routed(vec![]);
    assert_eq!(
        super::facts("llama-cpp", "qwen", Some(&not_llama), None, &[]),
        ServerFacts::default()
    );
}

#[test]
fn ollama_capabilities_report_the_thinking_switch_and_lm_studio_reports_none() {
    let url = routed(vec![(
        "/api/show",
        r#"{"capabilities":["completion","thinking"],"model_info":{}}"#.into(),
    )]);
    let facts = facts("ollama", "qwen3", Some(&url), None, &[]);
    assert_eq!(
        facts,
        ServerFacts {
            slots: None,
            thinking_switch: Some(true)
        }
    );
    let plain = routed(vec![(
        "/api/show",
        r#"{"capabilities":["completion"]}"#.into(),
    )]);
    assert_eq!(
        super::facts("ollama", "llama3", Some(&plain), None, &[]).thinking_switch,
        Some(false)
    );
    // LM Studio is never asked: its answer is always unknown here.
    let studio = routed(vec![("/api/v0/models", r#"{"data":[]}"#.into())]);
    assert_eq!(
        super::facts("lm-studio", "x", Some(&studio), None, &[]),
        ServerFacts::default()
    );
}

#[test]
fn hosted_routes_are_never_asked_for_facts_or_unloaded() {
    let hosted = "https://api.example.invalid/v1";
    assert_eq!(
        super::facts("llama-cpp", "m", Some(hosted), Some("secret"), &[]),
        ServerFacts::default()
    );
    assert_eq!(
        model_loaded("ollama", "m", hosted, Some("secret"), &[]),
        None
    );
    assert!(unload("ollama", "m", hosted, Some("secret"), &[]).is_err());
}

#[test]
fn model_loaded_reads_ollama_ps_lm_studio_state_and_llama_cpp() {
    let ollama = routed(vec![(
        "/api/ps",
        r#"{"models":[{"name":"qwen2.5:7b","model":"qwen2.5:7b"},{"name":"llama3:latest","model":"llama3:latest"}]}"#.into(),
    )]);
    assert_eq!(
        model_loaded("ollama", "qwen2.5:7b", &ollama, None, &[]),
        Some(true)
    );
    assert_eq!(
        model_loaded("ollama", "llama3", &ollama, None, &[]),
        Some(true),
        ":latest is implicit"
    );
    assert_eq!(
        model_loaded("ollama", "qwen2.5", &ollama, None, &[]),
        Some(false)
    );
    let studio = routed(vec![
        ("/api/v0/models/warm", r#"{"id":"warm","state":"loaded"}"#.into()),
        ("/api/v0/models/cold", r#"{"id":"cold","state":"not-loaded"}"#.into()),
        (
            "/api/v0/models",
            r#"{"data":[{"id":"qwen/qwen3","state":"loaded"},{"id":"other/x","state":"not-loaded"}]}"#.into(),
        ),
    ]);
    assert_eq!(
        model_loaded("lm-studio", "warm", &studio, None, &[]),
        Some(true)
    );
    assert_eq!(
        model_loaded("lm-studio", "cold", &studio, None, &[]),
        Some(false)
    );
    assert_eq!(
        model_loaded("lm-studio", "qwen/qwen3", &studio, None, &[]),
        Some(true)
    );
    assert_eq!(
        model_loaded("lm-studio", "other/x", &studio, None, &[]),
        Some(false)
    );
    assert_eq!(
        model_loaded("lm-studio", "missing", &studio, None, &[]),
        None
    );
    assert_eq!(model_loaded("lm-studio", "bad?x", &studio, None, &[]), None);
    let llama = routed(vec![(
        "/props",
        r#"{"default_generation_settings":{}}"#.into(),
    )]);
    assert_eq!(
        model_loaded("llama-cpp", "m", &llama, None, &[]),
        Some(true)
    );
    assert_eq!(
        model_loaded("llama-cpp", "m", &routed(vec![]), None, &[]),
        None
    );
}

/// One-shot server that records the request line and body, then answers `code` with `body`.
fn capture(code: u16, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let job = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut received = String::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
            if line == "\r\n" || line.is_empty() {
                break;
            }
            received.push_str(&line);
        }
        let mut body_bytes = vec![0u8; length];
        reader.read_exact(&mut body_bytes).unwrap();
        received.push_str(&String::from_utf8_lossy(&body_bytes));
        write!(stream, "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        received
    });
    (format!("http://{address}/v1"), job)
}

#[test]
fn ollama_unload_sends_keep_alive_zero_and_other_servers_have_no_unload() {
    let (url, job) = capture(200, r#"{"model":"llama3","done_reason":"unload"}"#);
    assert!(unload("ollama", "llama3", &url, None, &[]).unwrap());
    let request = job.join().unwrap();
    // The body is pretty-printed by the client; compare it without layout whitespace.
    let compact: String = request.split_whitespace().collect();
    assert!(request.starts_with("POST /api/generate "), "{request}");
    assert!(compact.contains(r#""keep_alive":0"#), "{request}");
    assert!(compact.contains(r#""model":"llama3""#), "{request}");
    // No request is made for a server without a documented unload.
    assert!(!unload("lm-studio", "m", "http://127.0.0.1:9/v1", None, &[]).unwrap());
    assert!(!unload("llama-cpp", "m", "http://127.0.0.1:9/v1", None, &[]).unwrap());
    // A refused unload is an error, never a silent success.
    let (url, job) = capture(500, "{}");
    assert!(unload("ollama", "llama3", &url, None, &[]).is_err());
    job.join().unwrap();
}

fn llama3_show() -> Value {
    json!({"model_info": {
        "general.architecture": "llama",
        "llama.block_count": 32,
        "llama.attention.head_count": 32,
        "llama.attention.head_count_kv": 8,
        "llama.embedding_length": 4096,
        "llama.context_length": 8192
    }})
}

#[test]
fn affordable_context_is_the_memory_budget_capped_at_the_native_window() {
    let shown = llama3_show();
    let weights = 4_500_000_000u64;
    let memory = 16u64 << 30;
    let expected = local_fit::max_context(
        weights,
        &Architecture::from_gguf(&GgufInfo {
            architecture: Some("llama".into()),
            block_count: Some(32),
            head_count: Some(32),
            head_count_kv: Some(8),
            embedding_length: Some(4096),
            context_length: Some(8192),
            ..GgufInfo::default()
        })
        .unwrap(),
        None,
        Some(memory),
    );
    assert!(expected > 0 && expected <= 8192, "{expected}");
    assert_eq!(
        affordable_from(&shown, weights, None, Some(memory)),
        Some(expected as u32)
    );
    assert_eq!(
        affordable_from(&shown, weights, None, None),
        None,
        "unknown memory"
    );
    assert_eq!(
        affordable_from(&shown, 64u64 << 30, None, Some(memory)),
        None,
        "weights exhaust the budget"
    );
    let no_layout = json!({"model_info": {"general.architecture": "llama"}});
    assert_eq!(
        affordable_from(&no_layout, weights, None, Some(memory)),
        None
    );
}

#[test]
fn affordable_context_needs_the_reported_size_from_ollama() {
    // /api/tags lists no size for the model, so the weights are unknown and no window is claimed.
    let url = routed(vec![
        ("/api/show", llama3_show().to_string()),
        (
            "/api/tags",
            r#"{"models":[{"name":"other:latest"}]}"#.into(),
        ),
    ]);
    assert_eq!(affordable_context("llama3", &url, None, &[], None), None);
}

#[test]
fn draft_candidates_are_smaller_same_tokenizer_models_nearest_first() {
    let root = tempfile::tempdir().unwrap();
    let mlx = |name: &str, weights: usize, tokenizer: &str| {
        let dir = root.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "{}").unwrap();
        std::fs::write(dir.join("model.safetensors"), vec![0u8; weights]).unwrap();
        std::fs::write(dir.join("tokenizer.json"), tokenizer).unwrap();
        dir
    };
    let same = r#"{"model":{"type":"BPE","vocab":{"a":0,"b":1},"merges":["a b"]}}"#;
    let target = mlx("target", 1000, same);
    let big = mlx("bigger", 2000, same);
    let near = mlx("near", 600, same);
    let far = mlx("far", 100, same);
    let other = mlx(
        "other",
        500,
        r#"{"model":{"type":"BPE","vocab":{"a":0,"c":1},"merges":[]}}"#,
    );
    let pool = vec![
        big.clone(),
        near.clone(),
        far.clone(),
        other,
        target.clone(),
    ];
    assert_eq!(
        draft_candidates_among(&target, &pool),
        vec![
            near.to_string_lossy().into_owned(),
            far.to_string_lossy().into_owned()
        ],
        "larger and different-tokenizer models are excluded, nearest size first"
    );
    let extra: Vec<PathBuf> = (0..4)
        .map(|i| mlx(&format!("small{i}"), 10 + i, same))
        .collect();
    assert_eq!(
        draft_candidates_among(&target, &extra).len(),
        MAX_DRAFT_CANDIDATES
    );
    // Hosted or unknown models have no draft.
    assert!(draft_candidates_among(Path::new("gpt-4o"), &pool).is_empty());
}

#[test]
fn mlx_tokenizer_digest_follows_content_not_formatting() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let c = tempfile::tempdir().unwrap();
    std::fs::write(
        a.path().join("tokenizer.json"),
        r#"{"model":{"vocab":{"x":0},"merges":[]}}"#,
    )
    .unwrap();
    std::fs::write(
        b.path().join("tokenizer.json"),
        "{\n \"model\": {\"vocab\": {\"x\": 0}, \"merges\": []}}\n",
    )
    .unwrap();
    std::fs::write(
        c.path().join("tokenizer.json"),
        r#"{"model":{"vocab":{"x":1},"merges":[]}}"#,
    )
    .unwrap();
    assert_eq!(
        mlx_tokenizer_digest(a.path()),
        mlx_tokenizer_digest(b.path())
    );
    assert!(mlx_tokenizer_digest(a.path()).is_some());
    assert_ne!(
        mlx_tokenizer_digest(a.path()),
        mlx_tokenizer_digest(c.path())
    );
    assert_eq!(mlx_tokenizer_digest(root_without_file().path()), None);
}

fn root_without_file() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn gguf_tokenizer_digest_is_the_token_list_only() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, tokens: &[&str], extra: Vec<u8>| {
        let path = dir.path().join(name);
        let mut array = 9u32.to_le_bytes().to_vec();
        array.extend(8u32.to_le_bytes());
        array.extend((tokens.len() as u64).to_le_bytes());
        for token in tokens {
            array.extend(string(token).get(4..).unwrap_or_default());
        }
        let mut entries = vec![
            ("general.architecture", string("llama")),
            ("tokenizer.ggml.tokens", array),
            ("general.file_type", extra),
        ];
        entries.sort_by(|a, b| a.0.cmp(b.0));
        std::fs::write(&path, header(&entries)).unwrap();
        path
    };
    let one = write(
        "one.gguf",
        &["a", "bc"],
        crate::llm::local_format::gguf_fixtures::uint32(1),
    );
    let two = write(
        "two.gguf",
        &["a", "bc"],
        crate::llm::local_format::gguf_fixtures::uint32(7),
    );
    let other = write(
        "other.gguf",
        &["a", "bd"],
        crate::llm::local_format::gguf_fixtures::uint32(1),
    );
    let digest = local_format::gguf_tokenizer_digest(&one);
    assert!(digest.is_some());
    assert_eq!(
        digest,
        local_format::gguf_tokenizer_digest(&two),
        "other keys do not matter"
    );
    assert_ne!(digest, local_format::gguf_tokenizer_digest(&other));
}
