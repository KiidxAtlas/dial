//! Headless text output and the process signal bridge. Interactive input belongs to `app`/`tui`.
use crate::{agent::Ui, platform, tools::ToolOutput};
use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

static INTERRUPT: AtomicBool = AtomicBool::new(false);

fn on_interrupt() {
    if INTERRUPT.swap(true, Ordering::SeqCst) {
        platform::sys::exit_now(130);
    }
}

pub fn install_sigint() -> Arc<AtomicBool> {
    platform::sys::on_interrupt(on_interrupt);
    let shared = Arc::new(AtomicBool::new(false));
    let mirror = shared.clone();
    std::thread::spawn(move || {
        // The thread's own clone is the last one once the caller drops its flag; stop then so
        // each `install_sigint` call does not leave a poller running for the rest of the process.
        while Arc::strong_count(&mirror) > 1 {
            // Never overwrite a cancellation raised by the raw-terminal event loop.
            if INTERRUPT.load(Ordering::SeqCst) {
                mirror.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
    shared
}

pub fn clear_interrupt() {
    INTERRUPT.store(false, Ordering::SeqCst);
}

pub struct TerminalUi {
    color: bool,
    streamed: bool,
    turn_streamed: bool,
    writer: Result<crate::output::StdoutWriter, io::Error>,
    write_error: Option<io::Error>,
    interrupt: Option<Arc<AtomicBool>>,
}
impl Default for TerminalUi {
    fn default() -> Self {
        Self::new()
    }
}
impl TerminalUi {
    pub fn new() -> Self {
        Self {
            color: io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
            streamed: false,
            turn_streamed: false,
            writer: crate::output::StdoutWriter::new(),
            write_error: None,
            interrupt: None,
        }
    }
    pub fn set_interrupt(&mut self, interrupt: Arc<AtomicBool>) {
        if let Ok(writer) = &mut self.writer {
            writer.set_interrupt(interrupt.clone());
        }
        self.interrupt = Some(interrupt);
        if self.check_output().is_err() {
            self.cancel_output();
        }
    }
    fn cancel_output(&self) {
        if let Some(interrupt) = &self.interrupt {
            interrupt.store(true, Ordering::SeqCst);
        }
    }
    pub fn check_output(&self) -> anyhow::Result<()> {
        if let Some(error) = &self.write_error {
            return Err(io::Error::new(error.kind(), error.to_string()).into());
        }
        match &self.writer {
            Err(error) => Err(io::Error::new(error.kind(), error.to_string()).into()),
            Ok(writer) => match writer.failure() {
                Some(error) => Err(error.into()),
                None => Ok(()),
            },
        }
    }
    /// Print mode's exit drain: wait (bounded) until this UI's queued stdout has reached the reader.
    pub fn drain_output(&self) -> io::Result<()> {
        match &self.writer {
            Ok(writer) => writer.drain(Some(std::time::Duration::from_secs(5))),
            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
        }
    }
    fn write_text(&mut self, bytes: &[u8]) {
        if self.check_output().is_err() {
            self.cancel_output();
            return;
        }
        if let Ok(writer) = &self.writer
            && let Err(error) = writer.text(bytes)
        {
            self.write_error = Some(error);
            self.cancel_output();
        }
    }
    fn write_json(&mut self, value: &serde_json::Value) -> io::Result<()> {
        let result = match &self.writer {
            Ok(writer) => writer.json(value),
            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
        };
        if result.is_err() {
            self.cancel_output();
        }
        result
    }
    fn dim(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[2m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
    pub fn finish_line(&mut self) {
        if self.streamed {
            self.write_text(b"\n");
            self.streamed = false;
        }
    }
    pub fn turn_streamed(&self) -> bool {
        self.turn_streamed
    }
}
impl Ui for TerminalUi {
    fn text_delta(&mut self, text: &str) {
        self.write_text(text.as_bytes());
        self.streamed = true;
        self.turn_streamed = true;
    }
    fn tool_call(&mut self, name: &str, summary: &str) {
        self.finish_line();
        let first = summary.lines().next().unwrap_or("");
        eprintln!(
            "{}",
            self.dim(&format!("● {name} {}", crate::agent::truncate(first, 140)))
        );
    }
    fn tool_result(&mut self, name: &str, output: &ToolOutput) {
        let lines: Vec<&str> = output.text.lines().collect();
        let shown: Vec<String> = lines
            .iter()
            .take(6)
            .map(|l| format!("  {}", crate::agent::truncate(l, 160)))
            .collect();
        let more = if lines.len() > 6 {
            format!("\n  … {} more lines", lines.len() - 6)
        } else {
            String::new()
        };
        let mark = if output.is_error { "✗" } else { "✓" };
        eprintln!(
            "{}",
            self.dim(&format!("{mark} {name}\n{}{more}", shown.join("\n")))
        );
    }
    fn notice(&mut self, message: &str) {
        self.finish_line();
        eprintln!("{}", self.dim(&format!("· {message}")));
    }
    // The trait's default approval denies: a headless process cannot ask the operator.
}

/// The stdout protocol for a non-interactive turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    StreamJson,
}

/// Carries a headless turn's status through the existing application entry point.
#[derive(Debug)]
pub struct PrintExit(pub i32);
impl std::fmt::Display for PrintExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "turn exited with status {}", self.0)
    }
}
impl std::error::Error for PrintExit {}

pub struct PrintUi {
    format: OutputFormat,
    terminal: TerminalUi,
    allowed: Vec<String>,
    denied: bool,
    text: String,
    calls: Vec<serde_json::Value>,
    prompt_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    completed: Option<bool>,
    efficiency: serde_json::Value,
    usage_breakdown: serde_json::Value,
    write_error: Option<io::Error>,
    print_thoughts: bool,
    reasoning: String,
}

impl PrintUi {
    /// Records a denial and explains how to lift it. Shared by name- and id-based results.
    fn note_denial(&mut self, name: &str, output: &ToolOutput) {
        if output.is_error && output.text.starts_with("denied") {
            self.denied = true;
            if output.text.starts_with("denied by a permission rule") {
                self.notice(&format!("Tool {name} was denied by a permission rule. Change that rule to permit it; --allow and --yes do not override deny rules."));
            } else if !output.text.starts_with("denied by the user") {
                self.notice(&format!("Tool {name} was denied. Use --approval ask --allow {name} to leave readonly mode and approve this tool."));
            }
        }
    }
    pub fn new(format: OutputFormat, allowed: Vec<String>) -> Self {
        Self {
            format,
            terminal: TerminalUi::new(),
            allowed,
            denied: false,
            text: String::new(),
            calls: Vec::new(),
            prompt_tokens: 0,
            output_tokens: 0,
            write_error: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            completed: None,
            efficiency: serde_json::Value::Null,
            usage_breakdown: serde_json::Value::Null,
            print_thoughts: false,
            reasoning: String::new(),
        }
    }
    /// `--print-thoughts`: write the turn's reasoning to stdout ahead of the answer (text mode only).
    pub fn set_print_thoughts(&mut self, enabled: bool) {
        self.print_thoughts = enabled;
    }
    pub fn set_interrupt(&mut self, interrupt: Arc<AtomicBool>) {
        self.terminal.set_interrupt(interrupt);
    }
    /// Check before starting inference and after finishing output; failures remain latched.
    pub fn check_output(&self) -> anyhow::Result<()> {
        if let Some(error) = &self.write_error {
            return Err(io::Error::new(error.kind(), error.to_string()).into());
        }
        self.terminal.check_output()
    }
    pub fn set_completed(&mut self, completed: bool) {
        self.completed = Some(completed);
    }
    pub(crate) fn set_usage_breakdown(
        &mut self,
        total: crate::llm::usage::Groups,
        turn: crate::llm::usage::Groups,
    ) {
        self.usage_breakdown = serde_json::json!({"total":total,"turn":turn});
    }

    fn emit(&mut self, value: serde_json::Value) {
        if self.write_error.is_some() {
            return;
        }
        let result = self.terminal.write_json(&value);
        if let Err(error) = result {
            self.write_error = Some(error);
        }
    }

    fn event(&mut self, value: impl FnOnce() -> serde_json::Value) {
        if self.format == OutputFormat::StreamJson {
            self.emit(value());
        }
    }

    pub fn system(&mut self, session: &str, model: &str, cwd: &std::path::Path, tools: &[String]) {
        self.event(|| serde_json::json!({"type":"system", "session_id":session, "model":model, "cwd":cwd, "tools":tools}));
    }

    pub fn finish(
        &mut self,
        report: Option<&crate::agent::TurnReport>,
        error: Option<&str>,
        session: Option<&str>,
        duration: std::time::Duration,
    ) -> anyhow::Result<i32> {
        self.check_output()?;
        let interrupted = report.is_some_and(|r| r.interrupted);
        let text = report.map_or(self.text.as_str(), |r| r.text.as_str());
        let code = if error.is_some() {
            1
        } else if self.denied {
            3
        } else if interrupted
            || text.trim().is_empty()
            || self.completed == Some(false)
            || report.is_some_and(|r| r.outcome != crate::agent::TurnOutcome::Completed)
        {
            2
        } else {
            0
        };
        if self.format == OutputFormat::Text {
            self.terminal.finish_line();
            if self.print_thoughts && !self.reasoning.trim().is_empty() {
                self.terminal
                    .write_text(self.reasoning.trim_end().as_bytes());
                self.terminal.write_text(b"\n");
            }
            if !self.terminal.turn_streamed() && !text.is_empty() {
                self.terminal.write_text(text.as_bytes());
                self.terminal.write_text(b"\n");
            }
        } else {
            let mut result = serde_json::json!({
                "type":"result", "ok":code == 0, "text":text,
                "steps":report.map_or(0, |r| r.steps), "interrupted":interrupted,
                "outcome":report.map(|r| r.outcome),
                "usage":{"prompt_tokens":self.prompt_tokens, "output_tokens":self.output_tokens, "cache_read_tokens":self.cache_read_tokens, "cache_write_tokens":self.cache_write_tokens, "total":self.usage_breakdown["total"], "turn":self.usage_breakdown["turn"]},
                "efficiency":self.efficiency,
                "duration_ms":duration.as_millis(), "tool_calls":self.calls, "session_id":session,
            });
            if let Some(error) = error {
                result["error"] = error.into();
            }
            self.emit(result);
        }
        self.terminal.drain_output()?;
        self.check_output()?;
        Ok(code)
    }
}

impl Ui for PrintUi {
    fn reasoning_delta(&mut self, text: &str) {
        self.reasoning.push_str(text);
        self.event(|| serde_json::json!({"type":"reasoning","delta":text}));
    }
    fn tool_arguments_delta(&mut self, index: u64, id: &str, name: &str, delta: &str) {
        self.event(|| serde_json::json!({"type":"tool_arguments","index":index,"id":id,"name":name,"delta":delta}));
    }
    fn cached_usage(&mut self, read: Option<u64>, write: Option<u64>) {
        if let Some(read) = read {
            self.cache_read_tokens = Some(self.cache_read_tokens.unwrap_or(0).saturating_add(read));
        }
        if let Some(write) = write {
            self.cache_write_tokens =
                Some(self.cache_write_tokens.unwrap_or(0).saturating_add(write));
        }
        self.event(
            || serde_json::json!({"type":"cached_usage","read_tokens":read,"write_tokens":write}),
        );
    }
    fn tool_output(&mut self, name: &str, text: &str) {
        if self.format == OutputFormat::Text {
            eprint!("{text}");
            if let Err(error) = io::stderr().flush() {
                self.write_error = Some(error);
                self.terminal.cancel_output();
            }
        }
        self.event(|| serde_json::json!({"type":"tool_output","name":name,"delta":text}));
    }
    fn background_notice(&mut self, message: &str) {
        if self.format == OutputFormat::Text {
            eprintln!("background: {message}");
        }
        self.event(|| serde_json::json!({"type":"background","message":message}));
    }
    fn extension_panel(&mut self, title: &str, markdown: &str) {
        if self.format == OutputFormat::Text {
            eprintln!("{title}\n{markdown}");
        }
        self.event(|| serde_json::json!({"type":"panel","title":title,"markdown":markdown}));
    }
    fn todo(&mut self, todo: &crate::agent::todo::TodoList) {
        self.event(|| serde_json::json!({"type":"todo","tasks":todo.snapshot()}));
    }
    fn text_delta(&mut self, text: &str) {
        if self.format != OutputFormat::Text {
            self.text.push_str(text);
        }
        if self.format == OutputFormat::Text {
            self.terminal.text_delta(text);
        }
        self.event(|| serde_json::json!({"type":"text", "delta":text}));
    }
    fn tool_call(&mut self, name: &str, summary: &str) {
        if self.format != OutputFormat::Text {
            self.calls.push(serde_json::json!({"name":name, "summary":summary, "is_error":false, "seconds":0.0}));
        }
        if self.format == OutputFormat::Text {
            self.terminal.tool_call(name, summary);
        }
        self.event(|| serde_json::json!({"type":"tool_call", "name":name, "summary":summary}));
    }
    fn tool_timing(&mut self, name: &str, seconds: f64) {
        if let Some(call) = self.calls.iter_mut().rev().find(|c| c["name"] == name) {
            call["seconds"] = seconds.into();
        }
    }
    fn tool_result(&mut self, name: &str, output: &ToolOutput) {
        let seconds = if let Some(call) = self.calls.iter_mut().rev().find(|c| c["name"] == name) {
            call["is_error"] = output.is_error.into();
            call["seconds"].as_f64().unwrap_or(0.0)
        } else {
            0.0
        };
        self.note_denial(name, output);
        if self.format == OutputFormat::Text {
            self.terminal.tool_result(name, output);
        }
        self.event(|| {
            serde_json::json!({
                "type":"tool_result", "name":name, "is_error":output.is_error, "seconds":seconds,
                "text":crate::agent::guard::clip_middle(&output.text, 16_384),
                "data":output.data,
            })
        });
    }
    fn tool_call_with_id(&mut self, call_id: &str, name: &str, summary: &str) {
        if self.format != OutputFormat::Text {
            self.calls.push(serde_json::json!({"call_id":call_id,"name":name,"summary":summary,"is_error":false,"seconds":0.0}));
        }
        if self.format == OutputFormat::Text {
            self.terminal.tool_call(name, summary);
        }
        self.event(|| serde_json::json!({"type":"tool_call","call_id":call_id,"name":name,"summary":summary}));
    }
    fn tool_result_with_id(&mut self, call_id: &str, name: &str, output: &ToolOutput) {
        let seconds = self
            .calls
            .iter_mut()
            .find(|call| call["call_id"] == call_id)
            .map(|call| {
                call["is_error"] = output.is_error.into();
                call["seconds"].as_f64().unwrap_or(0.0)
            })
            .unwrap_or(0.0);
        self.note_denial(name, output);
        if self.format == OutputFormat::Text {
            self.terminal.tool_result(name, output);
        }
        self.event(|| serde_json::json!({"type":"tool_result","call_id":call_id,"name":name,"is_error":output.is_error,"seconds":seconds,
            "text":crate::agent::guard::clip_middle(&output.text,16_384),"data":output.data}));
    }
    fn notice(&mut self, message: &str) {
        if self.format == OutputFormat::Text {
            self.terminal.notice(message);
        } else {
            eprintln!("dial: {message}");
        }
        self.event(|| serde_json::json!({"type":"notice", "message":message}));
    }
    fn usage(&mut self, prompt: Option<u64>, output: Option<u64>, seconds: f64) {
        self.prompt_tokens += prompt.unwrap_or(0);
        self.output_tokens += output.unwrap_or(0);
        self.event(|| serde_json::json!({"type":"usage", "prompt_tokens":prompt, "output_tokens":output, "seconds":seconds}));
    }
    fn efficiency(&mut self, report: &serde_json::Value) {
        self.efficiency = report.clone();
        self.event(|| serde_json::json!({"type":"efficiency","report":report}));
    }
    fn ask_questions(
        &mut self,
        request: &crate::agent::questions::QuestionRequest,
        _cancel: &AtomicBool,
    ) -> crate::agent::questions::QuestionOutcome {
        if self.check_output().is_err() {
            return crate::agent::questions::QuestionOutcome::Disconnected;
        }
        self.event(
            || serde_json::json!({"type":"question","request":request,"input":"unsupported"}),
        );
        crate::agent::questions::QuestionOutcome::Unsupported(
            "print mode has no human input channel; state the assumption and continue, or ask in an interactive session".into(),
        )
    }
    fn approve(&mut self, tool: &str, _summary: &str) -> crate::agent::Decision {
        if self.check_output().is_err() {
            self.denied = true;
            self.terminal.cancel_output();
            return crate::agent::Decision::Deny;
        }
        if self.allowed.iter().any(|name| name == tool) {
            crate::agent::Decision::Allow
        } else {
            self.denied = true;
            self.notice(&format!("Tool {tool} was denied. Use --allow {tool} to approve this tool, or --yes to approve all tools."));
            crate::agent::Decision::Deny
        }
    }
    fn approval_reason(&mut self) -> Option<String> {
        Some("Non-interactive approval was denied; the operator must pre-approve the tool with --allow.".into())
    }
}
