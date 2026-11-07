//! Child call admission, hook rewriting, execution and settlement.
use super::*;
use crate::llm::ToolCall;
use std::path::PathBuf;
use std::{
    cell::{Cell, RefCell},
    ops::ControlFlow,
};

type CallbackGate<'a> = std::sync::Mutex<crate::agent::safety::rules::CallbackGate<'a>>;

struct PreparedCall {
    call: llm::ToolCall,
    private_control: bool,
    memory_tool: bool,
}

/// Shared only among synchronous callbacks for this call. Each approver borrow is scoped
/// to a single policy operation; no callback is stored alongside data that it borrows.
struct CallContext<'a, 'owner, 'rules> {
    env: Env<'a>,
    job: &'a Job<'a>,
    surface: ToolSurface<'a>,
    approver: &'a RefCell<&'owner mut dyn Approver>,
    children: Option<&'a ChildBackends>,
    preview_abort: Option<&'a Cell<bool>>,
    callback_gate: Option<&'a CallbackGate<'rules>>,
    approval_denied: Cell<bool>,
}

fn refuse(text: String) -> ToolOutput {
    ToolOutput {
        execution: Default::default(),
        data: None,
        text,
        is_error: true,
    }
    .with_status(tools::ToolStatus::Blocked)
    .with_effects(crate::tools::surface::outcome::Effects::None)
}

pub(super) fn run(
    env: Env,
    job: &Job,
    surface: ToolSurface<'_>,
    call: &llm::ToolCall,
    approver: &mut dyn Approver,
    mut rerank: Option<RerankContext<'_>>,
    callback_gate: Option<&CallbackGate<'_>>,
) -> ToolOutput {
    let approver = RefCell::new(approver);
    let children = rerank.as_ref().map(|context| context.children);
    let preview_abort = rerank.as_ref().map(|context| context.preview_abort);
    let context = CallContext {
        env,
        job,
        surface,
        approver: &approver,
        children,
        preview_abort,
        callback_gate,
        approval_denied: Cell::new(false),
    };
    let prepared = match prepare_child_call(&context, call, &mut rerank) {
        ControlFlow::Continue(prepared) => prepared,
        ControlFlow::Break(output) => return output,
    };
    let original = &prepared.call;
    if matches!(original.name.as_str(), "find" | "evidence_search") {
        match crate::agent::model::roles::retrieval_request(
            env.workspace,
            &original.name,
            &original.arguments,
            env.interrupt,
        ) {
            Ok(Some(request)) => {
                let mut output = semantic_find(
                    env,
                    job,
                    surface,
                    SemanticCall {
                        call: original,
                        request,
                    },
                    rerank,
                    &approver,
                    callback_gate,
                );
                if let Some(reason) = gate_blocked(callback_gate) {
                    output.is_error = true;
                    output.text.push_str(&format!("\n[{reason}]"));
                }
                return output;
            }
            Ok(None) => {}
            Err(text) => return refuse(text),
        }
    }
    let code_tool = env
        .ext
        .get(&original.name)
        .filter(|tool| tool.supports_tool_dispatch())
        .cloned();
    let _serial = if code_tool.is_some()
        || matches!(original.name.as_str(), "edit" | "write")
        || env
            .ext
            .get(&original.name)
            .is_some_and(|tool| tool.manages_workspace_serialization())
        || approval::parallel_read(env.ext, &original.name, &original.arguments)
    {
        None
    } else {
        match env.workspace.serial_operation_checked(|| {
            if env.interrupt.load(Ordering::SeqCst) {
                Err(())
            } else {
                Ok(())
            }
        }) {
            Ok(guard) => Some(guard),
            Err(()) => return crate::agent::interrupted_tool(),
        }
    };
    if env.interrupt.load(Ordering::SeqCst) {
        return crate::agent::interrupted_tool();
    }
    if original.name == "edit"
        && env.workspace.edit_settings().streaming_abort
        && preview_reads_authorized(env, original, &approver)
    {
        let preview = tools::preview_edit(env.workspace, &original.arguments);
        if tools::preview_edit_should_abort(&preview) {
            if let Some(abort) = preview_abort {
                abort.set(true);
            }
            return refuse(format!(
                "Completed edit preview failed; no edit was executed: {}",
                preview.text
            ));
        }
    }
    let rewritten = match rewrite_child_call(&context, original) {
        Ok(rewritten) => rewritten,
        Err(reason) => return refuse(reason),
    };
    let call = rewritten.as_ref().unwrap_or(original);
    if env.interrupt.load(Ordering::SeqCst) {
        return crate::agent::interrupted_tool();
    }
    approver
        .borrow_mut()
        .tool_started(&call.id, &call.name, env.ext);
    if env.interrupt.load(Ordering::SeqCst) {
        approver.borrow_mut().tool_finished(&call.id);
        return crate::agent::interrupted_tool();
    }
    let output = execute_child_call(&context, original, call, &prepared, &mut rerank, code_tool);
    approver.borrow_mut().tool_finished(&call.id);
    finish_child_call(&context, call, output)
}

pub(super) struct GroupContext<'a, 'rules> {
    pub children: &'a ChildBackends,
    pub callback_gate: Option<&'a CallbackGate<'rules>>,
    pub pending: &'a mut Vec<String>,
    pub steps: u32,
}

/// Run an admitted native resource wave through the existing parallel executor. All policy and
/// computed-path callbacks remain on this child controller, including the live parent approver.
pub(super) fn run_group(
    env: Env<'_>,
    job: &Job<'_>,
    surface: ToolSurface<'_>,
    calls: &[ToolCall],
    approver: &mut dyn Approver,
    context: GroupContext<'_, '_>,
) -> Vec<(ToolOutput, f64)> {
    let GroupContext {
        children,
        callback_gate,
        pending,
        steps,
    } = context;
    let approver = RefCell::new(approver);
    let preview_abort = Cell::new(false);
    let contexts: Vec<_> = calls
        .iter()
        .map(|_| CallContext {
            env,
            job,
            surface,
            approver: &approver,
            children: Some(children),
            preview_abort: Some(&preview_abort),
            callback_gate,
            approval_denied: Cell::new(false),
        })
        .collect();
    let mut results = vec![None; calls.len()];
    let mut ready = Vec::new();
    let mut positions = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        match prepare_child_call(&contexts[index], call, &mut None) {
            ControlFlow::Break(output) => results[index] = Some((output, 0.0)),
            ControlFlow::Continue(prepared) => {
                if call.name == "edit"
                    && env.workspace.edit_settings().streaming_abort
                    && preview_reads_authorized(env, call, &approver)
                {
                    let preview = tools::preview_edit(env.workspace, &call.arguments);
                    if tools::preview_edit_should_abort(&preview) {
                        return calls.iter().map(|_| (refuse(format!("Native child wave not started: completed edit preview failed: {}",preview.text)),0.0)).collect();
                    }
                }
                ready.push(prepared.call);
                positions.push(index);
            }
        }
    }
    let bounds: Vec<_> = ready
        .iter()
        .map(|call| {
            crate::agent::run::dependency_dispatch::footprint_for(
                env.workspace,
                env.workspace.edit_mode(),
                env.ext,
                call,
            )
        })
        .collect();
    let mut authorize = |call: &ToolCall, tool: &str, paths: &[PathBuf], unbounded: bool| {
        let ready_index = ready
            .iter()
            .position(|c| c.id == call.id)
            .ok_or("unknown native child call")?;
        let source_read = matches!(call.name.as_str(), "edit" | "write") && tool == "read";
        if (tool != call.name && !source_read)
            || !bounds[ready_index]
                .as_ref()
                .is_some_and(|b| b.permits(paths, unbounded, source_read))
        {
            return Err("native child effect exceeded its admitted resource contract".into());
        }
        contexts[positions[ready_index]].authorize_mutation(call, tool, paths, unbounded)
    };
    let mut ui = RuleUi {
        observer: &approver,
        steps,
        stream: children.transcript.clone(),
        stream_error: None,
        registry: env.ext,
    };
    let outputs = crate::agent::run::dispatch::parallel_tools_with_mutations(
        crate::agent::ParallelToolContext {
            workspace: env.workspace,
            config: env.config,
            ext: env.ext,
            interrupt: env.interrupt,
            ui: &mut ui,
            pending_steering: pending,
        },
        &ready,
        None,
        None,
        Some(&mut authorize),
        Some(&children.cancellation.turn(env.interrupt.clone())),
        None,
    );
    for (index, (output, seconds)) in positions.into_iter().zip(outputs) {
        results[index] = Some((
            finish_child_call(&contexts[index], &calls[index], output),
            seconds,
        ));
    }
    results
        .into_iter()
        .map(|r| r.expect("every admitted child call settled"))
        .collect()
}

fn prepare_child_call(
    context: &CallContext<'_, '_, '_>,
    call: &llm::ToolCall,
    rerank: &mut Option<RerankContext<'_>>,
) -> ControlFlow<ToolOutput, PreparedCall> {
    let CallContext {
        env,
        job,
        surface,
        approver,
        children,
        preview_abort,
        callback_gate,
        ..
    } = context;
    let env = *env;
    let callback_gate = *callback_gate;
    let ToolSurface {
        schemas,
        names,
        provenance,
    } = *surface;
    let mut expanded = call.clone();
    if let Err(error) = crate::agent::window::file_handles::expand(env.workspace, &mut expanded) {
        return ControlFlow::Break(refuse(format!("File reference refused: {error}")));
    }
    let call = &expanded;
    if preview_abort.is_some_and(std::cell::Cell::get) {
        return ControlFlow::Break(refuse("This call was not executed: an authorized completed edit preview failed earlier in this assistant batch.".into()));
    }
    if matches!(call.name.as_str(), "edit" | "write")
        && call.arguments["verify"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty())
    {
        return ControlFlow::Break(refuse(format!(
            "{} verify requires the foreground controller's test runner; no mutation was executed. Child workers can run their scoped tests through bash.",
            call.name
        )));
    }
    if let Some((kind, why)) = call_form_error(call, schemas, names) {
        let help = (kind == "schema")
            .then(|| parameter_help(env, &call.name))
            .flatten()
            .map(|help| format!("\nThis call was not executed. {help}"))
            .unwrap_or_default();
        return ControlFlow::Break(refuse(format!("invalid tool call: {why}{help}")));
    }
    if let Some(context) = rerank.as_ref() {
        let journal = context.children.journal.borrow();
        if journal.pending.is_some() || journal.exploration.requested_rewind() {
            return ControlFlow::Break(refuse("private context transition already staged; further unstarted calls and callbacks are refused".into()));
        }
    }
    if let Some(reason) = gate_intercept(callback_gate, env.workspace.root(), call) {
        return ControlFlow::Break(refuse(reason));
    }
    if call.name == "task" {
        match crate::agent::safety::hooks::pre_tool_with(
            &env.config.hooks,
            env.workspace.root(),
            &call.name,
            &call.arguments,
            env.interrupt,
            &mut |notice| approver.borrow_mut().notice(notice),
        ) {
            Err(reason) => return ControlFlow::Break(refuse(reason)),
            Ok(Some(arguments)) if arguments != call.arguments => return ControlFlow::Break(refuse(
                "A spawn hook changed the task; submit revised arguments for a fresh policy check."
                    .into(),
            )),
            _ => {}
        }
        if let Err(denial) = approver.borrow_mut().authorize_from(
            call,
            &describe(call),
            approval::needs_approval(env.ext, call),
            provenance,
            env.interrupt,
        ) {
            return ControlFlow::Break(refuse(denial.message().into()));
        }
        if let Some(context) = rerank.as_mut() {
            context.backend.release();
        }
        let mut output = crate::agent::workers::subagents::nested(
            env,
            job,
            &call.arguments,
            names,
            &mut **approver.borrow_mut(),
        );
        crate::agent::safety::hooks::post_tool_with(
            &env.config.hooks,
            env.workspace.root(),
            &call.name,
            &call.arguments,
            &mut output,
            env.interrupt,
            &mut |notice| approver.borrow_mut().notice(notice),
        );
        return ControlFlow::Break(output);
    }
    if call.name == "message" {
        if provenance == approval::CallProvenance::RecoveredText
            && let Err(denial) = approver.borrow_mut().authorize_from(
                call,
                &describe(call),
                false,
                provenance,
                env.interrupt,
            )
        {
            return ControlFlow::Break(refuse(denial.message().into()));
        }
        return ControlFlow::Break(crate::agent::workers::live_tasks::output(
            approver.borrow_mut().peer_message(&call.arguments),
        ));
    }
    let private_control = matches!(
        call.name.as_str(),
        "context_notes" | "new_context" | "checkpoint" | "rewind"
    );
    // Provenance children (no selected runtime) run `memory` as an extension tool.
    let memory_action = crate::storage::memory_backend::call_action(&call.name, &call.arguments)
        .filter(|_| children.is_none_or(|children| children.memory.is_some()));
    let memory_tool = memory_action.is_some();
    if memory_action.is_some_and(|action| !crate::storage::memory_backend::reads(action)) {
        if env.plan_mode || !job.spec.mutates {
            return ControlFlow::Break(refuse("denied: selected memory mutation is unavailable during plan review or to a read-only child".into()));
        }
        match approver.borrow_mut().memory_capture_allowed(env.interrupt) {
            Ok(true) => {}
            Ok(false) => {
                return ControlFlow::Break(refuse(
                    "denied: current owner read-only/plan policy blocks selected memory mutation"
                        .into(),
                ));
            }
            Err(error) => {
                return ControlFlow::Break(refuse(format!(
                    "selected memory mutation authority unavailable: {error}"
                )));
            }
        }
    }
    let needs_approval = !private_control
        && if memory_tool {
            matches!(memory_action, Some("retain" | "learn"))
        } else {
            approval::needs_approval(env.ext, call)
        };
    if !job.spec.mutates && needs_approval {
        return ControlFlow::Break(refuse(read_only_refusal(&call.name)));
    }
    let summary = format!("[{}] {}", job.label, describe(call));
    if let Err(denial) = approver.borrow_mut().authorize_from(
        call,
        &summary,
        needs_approval,
        provenance,
        env.interrupt,
    ) {
        return ControlFlow::Break(refuse(denial.message().into()));
    }
    if env.interrupt.load(Ordering::SeqCst) {
        return ControlFlow::Break(crate::agent::interrupted_tool());
    }
    ControlFlow::Continue(PreparedCall {
        call: expanded,
        private_control,
        memory_tool,
    })
}

impl CallContext<'_, '_, '_> {
    fn authorize_mutation(
        &self,
        call: &llm::ToolCall,
        tool: &str,
        paths: &[std::path::PathBuf],
        unbounded: bool,
    ) -> std::result::Result<(), String> {
        let Self {
            env,
            job,
            approver,
            children,
            preview_abort,
            callback_gate,
            approval_denied,
            ..
        } = self;
        let callback_gate = *callback_gate;
        if env.interrupt.load(Ordering::SeqCst) {
            return Err("native effect cancelled before authorization".into());
        }
        if preview_abort.is_some_and(std::cell::Cell::get) {
            return Err(
                "native effect refused after an authorized completed edit preview failed".into(),
            );
        }
        if children.is_some_and(|children| {
            let journal = children.journal.borrow();
            journal.pending.is_some() || journal.exploration.requested_rewind()
        }) {
            return Err("private context transition staged before this effect".into());
        }
        if !job.spec.mutates {
            return Err("denied: read-only subagent cannot execute a native mutation".into());
        }
        if let Some(reason) = gate_blocked(callback_gate) {
            return Err(reason);
        }
        let scope = if tool == "read" {
            crate::agent::workers::subagents::MutationScope::SourceRead
        } else {
            crate::agent::workers::subagents::MutationScope::Workspace
        };
        let approved = approver.borrow_mut().authorize_mutation(
            call,
            tool,
            paths,
            unbounded,
            env.interrupt,
            scope,
        );
        if approved.is_err() {
            approval_denied.set(true);
        }
        approved?;
        if scope == crate::agent::workers::subagents::MutationScope::SourceRead {
            return Ok(());
        }
        let observed = crate::agent::safety::rules::mutation_call(
            env.workspace.root(),
            env.workspace.additional_roots(),
            call,
            paths,
            unbounded,
        )?;
        if let Some(reason) = gate_intercept(callback_gate, env.workspace.root(), &observed) {
            return Err(reason);
        }
        Ok(())
    }
    fn confirm(&self, call: &llm::ToolCall, summary: &str) -> bool {
        let Self {
            env,
            job,
            approver,
            approval_denied,
            ..
        } = self;
        if env.interrupt.load(Ordering::SeqCst) {
            return false;
        }
        let summary = format!("[{}] {summary}", job.label);
        let authorized = approver
            .borrow_mut()
            .authorize_with_cancel(call, &summary, true, env.interrupt)
            .is_ok();
        if !authorized {
            approval_denied.set(true);
        }
        authorized
    }
}

fn rewrite_child_call(
    context: &CallContext<'_, '_, '_>,
    call: &llm::ToolCall,
) -> std::result::Result<Option<llm::ToolCall>, String> {
    let CallContext {
        env,
        surface,
        approver,
        callback_gate,
        ..
    } = context;
    let callback_gate = *callback_gate;
    let ToolSurface {
        schemas,
        names,
        provenance,
    } = *surface;
    let root = env.workspace.root();
    let mut rewritten;
    match crate::agent::safety::hooks::pre_tool_with(
        &env.config.hooks,
        root,
        &call.name,
        &call.arguments,
        env.interrupt,
        &mut |notice| approver.borrow_mut().notice(notice),
    ) {
        Err(reason) => {
            approver.borrow_mut().policy_explanation(
                call,
                &approval::PolicyExplanation {
                    reason: approval::PolicyReason::HookVeto,
                    rule_scope: None,
                },
            );
            Err(reason)
        }
        Ok(None) => Ok(None),
        Ok(Some(arguments)) => {
            // Only schema-valid read-only rewrites are supported here. The original
            // authorization does not cover a rewritten path or other policy inputs.
            rewritten = llm::ToolCall {
                arguments,
                ..call.clone()
            };
            if let Err(error) =
                crate::agent::window::file_handles::expand(env.workspace, &mut rewritten)
            {
                return Err(format!("File reference refused: {error}"));
            }
            if approval::needs_approval(env.ext, &rewritten)
                || call_form_error(&rewritten, schemas, names).is_some()
            {
                return Err("A pre_tool_use hook rewrote this call into one that needs approval or fails its schema; subagents only run rewritten read-only calls. Retry with arguments the hook accepts unchanged.".into());
            }
            if let Err(denial) = approver.borrow_mut().authorize_from(
                &rewritten,
                &summarize(&rewritten),
                false,
                provenance,
                env.interrupt,
            ) {
                return Err(denial.message().into());
            }
            if let Some(reason) = gate_intercept(callback_gate, env.workspace.root(), &rewritten) {
                return Err(reason);
            }
            Ok(Some(rewritten))
        }
    }
}

fn execute_child_call(
    context: &CallContext<'_, '_, '_>,
    original: &llm::ToolCall,
    call: &llm::ToolCall,
    prepared_call: &PreparedCall,
    rerank: &mut Option<RerankContext<'_>>,
    code_tool: Option<std::sync::Arc<dyn crate::tools::integrations::ext::Tool>>,
) -> ToolOutput {
    let CallContext {
        env,
        job,
        surface,
        approver,
        children,
        preview_abort,
        callback_gate,
        ..
    } = context;
    let env = *env;
    let surface = *surface;
    let callback_gate = *callback_gate;
    let ToolSurface { schemas, names, .. } = surface;
    let private_control = prepared_call.private_control;
    let memory_tool = prepared_call.memory_tool;
    // Mutation/confirmation policy stays bound to the original authorized call, as before.
    let authorize_mutation = |tool: &str, paths: &[std::path::PathBuf], unbounded: bool| {
        context.authorize_mutation(original, tool, paths, unbounded)
    };
    let confirm = |summary: &str| context.confirm(original, summary);
    let prepared = children
        .and_then(|children| child_speculation_key(env, job, call, &children.namespace))
        .and_then(|key| {
            env.workspace
                .speculation
                .take_named(env.workspace, &call.name, &call.arguments, &key)
        });
    if let Some(output) = prepared {
        output
    } else if let Some(tool) = code_tool {
        let mut callbacks = 0usize;
        let mut questions = Vec::new();
        let mut dispatch = |request| {
            if env.interrupt.load(Ordering::SeqCst) {
                return crate::agent::interrupted_tool();
            }
            if preview_abort.is_some_and(std::cell::Cell::get) {
                return ToolOutput::err(
                    "code-mode callbacks are blocked after an authorized completed edit preview failure",
                );
            }
            if let Some(reason) = gate_blocked(callback_gate) {
                return ToolOutput::err(reason);
            }
            callbacks += 1;
            if callbacks > 128 {
                return ToolOutput::err("code-mode callback limit exceeded (128 per cell)");
            }
            match request {
                crate::tools::integrations::ext::ToolCallback::Schemas => ToolOutput {
                    execution: Default::default(),
                    data: Some(Value::Array(
                        schemas
                            .iter()
                            .filter(|schema| {
                                schema["function"]["name"].as_str().is_some_and(|name| {
                                    !matches!(name, "task" | "finish" | "exit_plan")
                                        && !env
                                            .ext
                                            .get(name)
                                            .is_some_and(|tool| tool.supports_tool_dispatch())
                                })
                            })
                            .cloned()
                            .collect(),
                    )),
                    text: String::new(),
                    is_error: false,
                },
                crate::tools::integrations::ext::ToolCallback::Call { name, arguments } => {
                    if env
                        .ext
                        .get(&name)
                        .is_some_and(|tool| tool.supports_tool_dispatch())
                        || matches!(name.as_str(), "finish" | "exit_plan")
                    {
                        return ToolOutput::err(
                            "recursive code-mode and turn-control callbacks are not available",
                        );
                    }
                    let nested = llm::ToolCall {
                        id: String::new(),
                        name,
                        arguments,
                        malformed_arguments: false,
                    };
                    let context = rerank.as_mut().map(|context| RerankContext {
                        backend: &mut *context.backend,
                        adapter: context.adapter,
                        steps: &mut *context.steps,
                        budget: context.budget,
                        children: context.children,
                        preview_abort: context.preview_abort,
                        capture: context.capture,
                    });
                    approver.borrow_mut().notice(&format!(
                        "[{} callback] {}",
                        call.name,
                        summarize(&nested)
                    ));
                    // The shared gate runs once in call_tool, before nested authorization.
                    let mut output = call_tool(
                        env,
                        job,
                        surface,
                        &nested,
                        &mut **approver.borrow_mut(),
                        context,
                        callback_gate,
                    );
                    let image = match crate::agent::callback_image(
                        env.workspace,
                        &nested,
                        &mut output,
                        None,
                    ) {
                        Ok(image) => image,
                        Err(error) => {
                            output.is_error = true;
                            output.text.push_str(&format!(
                                "\n[Preparing callback image evidence failed: {error:#}]"
                            ));
                            None
                        }
                    };
                    approver.borrow_mut().notice(&format!(
                        "[{} callback result] {}",
                        call.name,
                        crate::agent::truncate(&output.text, 2000)
                    ));
                    if let Err(error) = crate::agent::callback_image_payload(&mut output, image) {
                        output.is_error = true;
                        output.text.push_str(&format!(
                            "\n[Encoding callback image payload failed: {error:#}]"
                        ));
                    }
                    if nested.name == "ask" {
                        questions.push(json!({"parent":call.name,"arguments":nested.arguments,"text":output.text,"data":output.data,"is_error":output.is_error}));
                    }
                    approver
                        .borrow_mut()
                        .tool_started(&call.id, &call.name, env.ext);
                    output
                }
            }
        };
        let mut output = crate::tools::exec::execution::with_dispatch(
            env.ext,
            tool.as_ref(),
            &ToolContext {
                workspace: env.workspace,
                config: env.config,
                mutation_authorizer: Some(&authorize_mutation),
                confirm: Some(&confirm),
            },
            &call.arguments,
            &|_| {},
            env.interrupt,
            &mut dispatch,
        );
        if !questions.is_empty() {
            if !output.data.as_ref().is_some_and(Value::is_object) {
                output.data = Some(json!({"code_output":output.data.take()}));
            }
            output.data.as_mut().expect("object code result")["dial_question_results"] =
                json!(questions);
        }
        output
    } else if matches!(call.name.as_str(), "edit" | "write") {
        let context = std::cell::RefCell::new(rerank.as_mut());
        let repair = |request: &tools::EditRepairRequest| -> std::result::Result<String, String> {
            let mut context = context.borrow_mut();
            let context = context
                .as_mut()
                .ok_or("edit repair requires the active child model controller")?;
            if !env.config.model_roles.contains_key("coach") {
                return Err("edit repair requires a configured coach model role".into());
            }
            let chat =
                crate::agent::run::edit_runtime::repair_request(request, env.harness.max_tokens);
            let text = child_role_chat(
                env,
                context.backend,
                context.adapter,
                ChildRoleCall {
                    role: "coach",
                    purpose: llm::usage::Purpose::Repair,
                    request: &chat,
                    steps: context.steps,
                    budget: context.budget,
                    model_override: None,
                    timeout: None,
                    operation: None,
                },
                approver,
                context.children,
            )
            .map_err(|error| format!("{error:#}"))?;
            let response = crate::agent::run::edit_runtime::repair_response(&text);
            if response.is_err() {
                with_child_role_backend(
                    env,
                    context.backend,
                    "coach",
                    llm::usage::Purpose::Repair,
                    None,
                    context.children,
                    |backend, _, _| backend.record_discarded_response(),
                )
                .map_err(|error| format!("{error:#}"))?;
            }
            response
        };
        let ctx = ToolContext {
            workspace: env.workspace,
            config: env.config,
            mutation_authorizer: Some(&authorize_mutation),
            confirm: Some(&confirm),
        };
        if call.name == "edit" {
            tools::edit_with_context(&ctx, &call.arguments, Some(&repair), env.interrupt)
        } else {
            tools::write_with_context(&ctx, &call.arguments, Some(&repair), env.interrupt)
        }
    } else if private_control || call.name == "session_search" {
        let result = (|| -> Result<ToolOutput> {
            let children = rerank
                .as_ref()
                .context("private history tools require the actual child controller")?
                .children;
            let mut journal = children.journal.borrow_mut();
            match call.name.as_str() {
                "context_notes" => {
                    let mut output = crate::agent::window::context_notes::execute_notes(
                        &mut journal.session,
                        &call.arguments,
                        &children.namespace,
                        env.config.compaction.experimental_context_management,
                        env.interrupt,
                    );
                    if let Some(data) = output.data.as_mut().and_then(Value::as_object_mut) {
                        data.remove("durable");
                        data.insert("private_task_lifetime".into(), json!(true));
                    }
                    output.text.push_str(
                        "\n[Private child journal: available only until this task ends.]",
                    );
                    Ok(output)
                }
                "new_context" => {
                    anyhow::ensure!(
                        !call.id.is_empty(),
                        "new_context requires a real accepted native tool call and paired result; it is not a code callback"
                    );
                    anyhow::ensure!(
                        !journal.exploration.busy(),
                        "rewind the active investigation before new_context"
                    );
                    let (output, request) =
                        crate::agent::window::context_notes::request_new_context(
                            &journal.session,
                            &call.arguments,
                            &children.namespace,
                            &call.id,
                            env.config.compaction.experimental_context_management,
                            env.interrupt,
                        )?;
                    journal.pending = Some(request);
                    journal.control_call = Some(call.clone());
                    Ok(output)
                }
                "checkpoint" | "rewind" => {
                    anyhow::ensure!(
                        env.config.checkpoint.enabled,
                        "private checkpoint tools are disabled"
                    );
                    anyhow::ensure!(
                        ["checkpoint", "rewind", "session_search"]
                            .iter()
                            .all(|required| names.iter().any(|name| name == required)),
                        "private checkpoint requires both lifecycle tools and permitted raw session_search recovery"
                    );
                    let policy = child_policy(env, job, schemas, &journal.rule_policy)?;
                    let output = journal.exploration.request(
                        call,
                        &children.namespace,
                        policy,
                        crate::storage::session::Session::exploration_timestamp(),
                        env.interrupt,
                    );
                    if !output.is_error {
                        journal.control_call = Some(call.clone());
                    }
                    Ok(output)
                }
                "session_search" if call.arguments.get("view").is_some() => {
                    let context = rerank
                        .as_ref()
                        .context("layer recovery requires the actual child controller")?;
                    let selection = env.config.selection();
                    Ok(crate::agent::compact::snapcompact::views::call(
                        &journal.session.messages(),
                        &call.arguments,
                        crate::agent::compact::snapcompact::views::ViewContext {
                            policy: &env.config.compaction,
                            identity: crate::config::compaction::CompactionIdentity {
                                provider: &selection.provider,
                                model: &selection.model,
                                agent: Some(job.kind),
                            },
                            window: child_window(env, context.backend),
                            vision: context.backend.accepts_image_input(),
                            family: context.backend.compaction_snapcompact_family(),
                        },
                        env.interrupt,
                    ))
                }
                "session_search" => Ok(crate::storage::session::session_search_tool::call_owned(
                    Some(&children.layout),
                    &journal.session,
                    &call.arguments,
                )),
                _ => unreachable!(),
            }
        })();
        result.unwrap_or_else(|error| ToolOutput::err(format!("{error:#}")))
    } else if call.name == "ask" {
        match children
            .context("ask requires the actual private child identity")
            .and_then(|children| {
                crate::agent::workflow::questions::prepare(
                    &call.arguments,
                    env.config,
                    false,
                    &children.namespace,
                )
                .map_err(anyhow::Error::msg)
            }) {
            Ok(request) => {
                let outcome = approver.borrow_mut().ask_questions(&request, env.interrupt);
                crate::agent::workflow::questions::output(&request, outcome, env.interrupt)
            }
            Err(error) => ToolOutput::err(format!("{error:#}")),
        }
    } else if memory_tool {
        let result = (|| -> Result<ToolOutput> {
            let context = rerank
                .as_mut()
                .context("memory tools require the genuine child controller")?;
            let memory = context
                .children
                .memory
                .as_ref()
                .context("selected child memory backend is not active")?;
            let authority = context.children.observe_memory()?;
            let mut runner = ChildMemoryRunner::new(
                env,
                context.backend,
                context.adapter,
                context.steps,
                context.budget,
                approver,
                context.children,
                Some(call),
                job.spec.mutates,
                context.capture,
            );
            *context.children.memory_pending.borrow_mut() = Some(call.clone());
            let data = memory.execute_tool(&authority, &call.arguments, &mut runner)?;
            Ok(ToolOutput {
                execution: Default::default(),
                text: serde_json::to_string_pretty(&data)?,
                data: Some(data),
                is_error: false,
            })
        })();
        result.unwrap_or_else(|error| ToolOutput::err(format!("{error:#}")))
    } else {
        execute(env, call, Some(&authorize_mutation), Some(&confirm))
    }
}

fn finish_child_call(
    context: &CallContext<'_, '_, '_>,
    call: &llm::ToolCall,
    mut output: ToolOutput,
) -> ToolOutput {
    let CallContext {
        env,
        approver,
        preview_abort,
        callback_gate,
        approval_denied,
        children,
        ..
    } = context;
    let callback_gate = *callback_gate;
    let root = env.workspace.root();
    if let Some(reason) = gate_blocked(callback_gate) {
        output.is_error = true;
        output.text.push_str(&format!("\n[{reason}]"));
    }
    if preview_abort.is_some_and(std::cell::Cell::get) {
        output.is_error = true;
        output.text.push_str("\n[An authorized completed edit preview failed; further callbacks and unstarted batch actions were refused.]");
    }
    if !approval_denied.get() {
        crate::agent::safety::hooks::post_tool_with(
            &env.config.hooks,
            root,
            &call.name,
            &call.arguments,
            &mut output,
            env.interrupt,
            &mut |notice| approver.borrow_mut().notice(notice),
        );
    }
    if output.is_error
        && let Some(children) = children
    {
        let mut journal = children.journal.borrow_mut();
        journal.exploration.reject_result(call);
        if journal
            .pending
            .as_ref()
            .is_some_and(|request| request.tool_call_id.as_deref() == Some(&call.id))
        {
            journal.pending = None;
            journal.control_call = None;
        }
    }
    output
}
