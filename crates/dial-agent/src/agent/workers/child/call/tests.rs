use super::*;
use crate::agent::workers::subagents;
use crate::config::types::{Config, Harness};
use crate::tools::exec::eval::BATCH_CALLBACK;
use crate::tools::integrations::ext::{Registry, Tool, ToolCallback, ToolEvent};
use crate::tools::{Workspace, exec::background::AdoptedJob};
use std::sync::{Arc, atomic::AtomicBool};

/// A code-mode tool whose cell sends each `batches` entry as one `dial.batch` callback and returns
/// the ordered receipts.
struct Cell;

impl Tool for Cell {
    fn name(&self) -> &str {
        "cell"
    }
    fn schema(&self) -> Value {
        json!({"type":"function","function":{"name":"cell","description":"fake code cell","parameters":{"type":"object","properties":{"batches":{"type":"array"}}}}})
    }
    fn needs_approval(&self, _: &Value) -> bool {
        false
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn supports_tool_dispatch(&self) -> bool {
        true
    }
    fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
        ToolOutput::err("cell needs a dispatcher")
    }
    fn call_with_dispatch(
        &self,
        _: &ToolContext,
        args: &Value,
        _: &(dyn Fn(ToolEvent) + Sync),
        _: &AtomicBool,
        _: &dyn Fn(AdoptedJob) -> Result<String, String>,
        dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
    ) -> ToolOutput {
        let receipts: Vec<Value> = args["batches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|calls| {
                let output = dispatch(ToolCallback::Call {
                    name: BATCH_CALLBACK.into(),
                    arguments: json!({ "calls": calls }),
                });
                json!({"is_error":output.is_error,"text":output.text,"data":output.data})
            })
            .collect();
        ToolOutput {
            execution: Default::default(),
            data: Some(Value::Array(receipts)),
            text: String::new(),
            is_error: false,
        }
    }
}

struct Deploy;

impl Tool for Deploy {
    fn name(&self) -> &str {
        "deploy"
    }
    fn schema(&self) -> Value {
        json!({"type":"function","function":{"name":"deploy","description":"fake","parameters":{"type":"object","properties":{}}}})
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
        ToolOutput::ok("deployed".into())
    }
}

/// Denies every prompt and records which tools started.
#[derive(Default)]
struct Reviewer {
    asked: Vec<String>,
    started: Vec<String>,
}

impl Approver for Reviewer {
    fn authorize(
        &mut self,
        call: &llm::ToolCall,
        _: &str,
        needs_approval: bool,
    ) -> Result<(), approval::Denial> {
        if needs_approval {
            self.asked.push(call.name.clone());
            Err(approval::Denial::User)
        } else {
            Ok(())
        }
    }
    fn tool_started(&mut self, _: &str, name: &str, _: &Registry) {
        self.started.push(name.into());
    }
}

fn run_cell(batches: Value) -> (ToolOutput, Reviewer) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (config, mut harness, interrupt) = (
        Config::default(),
        Harness::default(),
        Arc::new(AtomicBool::new(false)),
    );
    harness
        .tools
        .extend(["cell".into(), "deploy".into(), "read".into()]);
    let mut ext = Registry::new();
    ext.register(Arc::new(Cell));
    ext.register(Arc::new(Deploy));
    let env = Env {
        config: &config,
        workspace: &workspace,
        ext: &ext,
        layout: None,
        harness: &harness,
        interrupt: &interrupt,
        usage: None,
        context_cap: None,
        memory: None,
        agents_messages: &[],
        plan_mode: false,
    };
    let request = subagents::parse(
        &json!({"agent": "worker", "task": "p", "solution_space": "s"}),
        &config,
    )
    .unwrap();
    let job = &request.jobs[0];
    let names = toolset(job.spec, &harness.tools, &ext);
    assert!(
        ["cell", "deploy", "read"]
            .iter()
            .all(|name| names.iter().any(|offered| offered == name)),
        "{names:?}"
    );
    let schemas = schemas(env, &names).unwrap();
    let call = llm::ToolCall {
        id: "cell-1".into(),
        name: "cell".into(),
        arguments: json!({ "batches": batches }),
        malformed_arguments: false,
    };
    let mut reviewer = Reviewer::default();
    let output = call_tool(
        env,
        job,
        ToolSurface {
            schemas: &schemas,
            names: &names,
            provenance: approval::CallProvenance::Structured,
        },
        &call,
        &mut reviewer,
        None,
        None,
    );
    (output, reviewer)
}

fn receipts(output: &ToolOutput) -> Vec<Value> {
    assert!(!output.is_error, "{}", output.text);
    output.data.as_ref().unwrap().as_array().unwrap().clone()
}

#[test]
fn worker_cells_run_read_only_batches_together_in_entry_order() {
    let (output, reviewer) = run_cell(json!([[
        {"name":"read","arguments":{"path":"b.txt"}},
        {"name":"read","arguments":{"path":"missing.txt"}},
        {"name":"read","arguments":{"path":"a.txt"}},
    ]]));
    let receipts = receipts(&output);
    assert_eq!(receipts[0]["is_error"], false, "the batch is supported");
    let results = receipts[0]["data"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert!(results[0]["text"].as_str().unwrap().contains("beta"));
    assert_eq!(
        results[1]["is_error"], true,
        "a failed entry keeps its slot"
    );
    assert!(results[2]["text"].as_str().unwrap().contains("alpha"));
    assert!(reviewer.asked.is_empty(), "reads need no prompt");
    assert_eq!(
        reviewer
            .started
            .iter()
            .filter(|name| *name == "read")
            .count(),
        3,
        "the parallel group reports each started read: {:?}",
        reviewer.started
    );
}

#[test]
fn worker_batches_with_an_approval_entry_run_serially_and_never_run_denied_entries() {
    let (output, reviewer) = run_cell(json!([
        [
            {"name":"read","arguments":{"path":"a.txt"}},
            {"name":"deploy","arguments":{}},
        ],
        [{"name":BATCH_CALLBACK,"arguments":{"calls":[]}}],
        "not entries",
    ]));
    let receipts = receipts(&output);
    let mixed = receipts[0]["data"].as_array().unwrap();
    assert!(mixed[0]["text"].as_str().unwrap().contains("alpha"));
    assert_eq!(mixed[1]["is_error"], true);
    assert!(!mixed[1]["text"].as_str().unwrap().contains("deployed"));
    assert_eq!(reviewer.asked, ["deploy"], "the serial entry asked once");
    assert_eq!(
        receipts[1]["data"][0]["is_error"], true,
        "batches never nest"
    );
    assert_eq!(
        receipts[2]["is_error"], true,
        "malformed payloads are refused"
    );
}

#[test]
fn worker_mixed_batches_group_the_reads_on_each_side_of_a_serial_entry() {
    let (output, reviewer) = run_cell(json!([[
        {"name":"read","arguments":{"path":"a.txt"}},
        {"name":"read","arguments":{"path":"b.txt"}},
        {"name":"deploy","arguments":{}},
        {"name":"read","arguments":{"path":"b.txt"}},
        {"name":"read","arguments":{"path":"a.txt"}},
    ]]));
    let receipts = receipts(&output);
    let results = receipts[0]["data"].as_array().unwrap();
    assert_eq!(results.len(), 5);
    for (index, expected) in [(0, "alpha"), (1, "beta"), (3, "beta"), (4, "alpha")] {
        assert!(
            results[index]["text"].as_str().unwrap().contains(expected),
            "entry {index} keeps its slot: {}",
            results[index]["text"]
        );
    }
    assert_eq!(results[2]["is_error"], true);
    assert_eq!(reviewer.asked, ["deploy"], "only the serial entry asked");
    // Each shared read group starts its reads together, then hands progress back to the parent
    // cell when it drains; the denied serial entry between them never starts.
    assert_eq!(
        reviewer.started,
        [
            "cell", "read", "read", "cell", "cell", "read", "read", "cell"
        ],
        "two read groups ran around the serial entry"
    );
}

#[test]
fn worker_batch_entries_count_against_the_cell_callback_limit() {
    let read = json!({"name":"read","arguments":{"path":"a.txt"}});
    let full: Vec<Value> = (0..CODE_CALLBACK_LIMIT).map(|_| read.clone()).collect();
    let (output, _) = run_cell(json!([[read.clone()], full]));
    let receipts = receipts(&output);
    assert_eq!(receipts[0]["is_error"], false);
    assert_eq!(receipts[1]["is_error"], true);
    assert!(
        receipts[1]["text"]
            .as_str()
            .unwrap()
            .contains("callback limit exceeded"),
        "{}",
        receipts[1]["text"]
    );
}
