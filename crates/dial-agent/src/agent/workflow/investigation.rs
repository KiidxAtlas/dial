//! Bounded, turn-local detection of successful observations that add no new evidence.
//! Execute first: a source may have changed externally, and a reread can be legitimate.
use crate::llm::ToolCall;
use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
};

#[derive(Default)]
pub(in crate::agent) struct InvestigationGuard {
    observations: VecDeque<(u64, String, usize)>,
    unchanged: usize,
}

impl InvestigationGuard {
    pub fn observe_output(
        &mut self,
        call: &ToolCall,
        output: &crate::runtime::tool_output::ToolOutput,
    ) -> Option<String> {
        if !output.execution.changed_paths.is_empty() {
            self.observations.clear();
            self.unchanged = 0;
        }
        self.observe(call, &output.text, output.is_error)
    }
    pub fn observe(&mut self, call: &ToolCall, text: &str, is_error: bool) -> Option<String> {
        // A failed check is not progress and must not erase earlier duplicate evidence.
        if is_error {
            return None;
        }
        match call.name.as_str() {
            // Successful edits allow fresh verification. Commands alone do not prove
            // a change; observations are executed again and compared with their result.
            "write" | "edit" | "ast_edit" => {
                return None;
            }
            // Reading an archived artifact:// recovers earlier output; it is not a new observation.
            "read"
                if call.arguments["path"]
                    .as_str()
                    .is_some_and(|path| path.starts_with("artifact://")) =>
            {
                return None;
            }
            "read" | "grep" | "glob" | "find" | "ast_search" | "bash" => {}
            // Polls, artifact recovery and control calls are not duplicate observations.
            _ => return None,
        }
        let key = observation_key(call, text);
        if let Some(advice) = self.repeat_known(key) {
            return Some(advice);
        }
        self.unchanged = 0;
        if self.observations.len() == 128 {
            self.observations.pop_front();
        }
        self.observations.push_back((key, call.id.clone(), 0));
        None
    }

    /// Forgets observations whose result the model no longer sees in full (compacted, pruned or
    /// superseded). A repeat of such a result is new evidence: it executes and returns full content.
    pub fn retain_visible(&mut self, visible: impl Fn(&str) -> bool) {
        self.observations.retain(|(_, id, _)| visible(id.as_str()));
    }

    /// Retains observations whose results remain verbatim after compaction, resetting repeat streaks.
    pub fn compact_retaining(&mut self, visible: impl Fn(&str) -> bool) {
        self.retain_visible(visible);
        for (_, _, repeats) in &mut self.observations {
            *repeats = 0;
        }
        self.unchanged = 0;
    }

    /// Releases all duplicate memory, e.g. after compaction rewrote the context.
    pub fn forget(&mut self) {
        self.observations.clear();
        self.unchanged = 0;
    }

    /// Count a retry of cached evidence without pretending the withheld tool executed.
    pub fn repeat_known(&mut self, key: u64) -> Option<String> {
        let (_, first, repeats) = self
            .observations
            .iter_mut()
            .find(|(seen, _, _)| *seen == key)?;
        *repeats += 1;
        self.unchanged = *repeats;
        Some(format!(
            "[Investigation check from dial] This observation is unchanged from tool call {first}. Reuse that evidence; do not reread it for reassurance. If the answer supports a change and its check, implement and verify now, or deliver the requested finding. If unresolved, choose one narrower check that distinguishes the remaining possibilities. Reread only for changed sources, missing context or a specific verification need."
        ))
    }

    /// Several unchanged observations are not further investigation. Intervening reads, commands and
    /// failures cannot reset an observation's repeat count. Guidance escalates at each further multiple
    /// of four and never ends the turn: the repeated observation is already withheld.
    pub fn escalation(&self) -> Option<String> {
        (self.unchanged >= 4 && self.unchanged.is_multiple_of(4)).then(|| format!(
            "[Escalation from dial] This observation has now been repeated {} times unchanged and withheld each time. Its evidence is already in this conversation. Stop re-checking it: act on it, or run one check of a different kind.",
            self.unchanged
        ))
    }
}

/// Shared result identity for duplicate detection and completion progress.
/// Source-tagged rows identify their own path/range; storage handles and range
/// footers are not new evidence. Unstructured observations retain their arguments.
pub(in crate::agent) fn observation_key(call: &ToolCall, text: &str) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    call.name.hash(&mut hash);
    let anchored_read = call.name == "read"
        && text
            .lines()
            .next()
            .is_some_and(|line| source_header(line).is_some());
    if !anchored_read {
        crate::agent::safety::guard::normalized_arguments(call)
            .to_string()
            .hash(&mut hash);
    }
    for line in text.lines().filter(|line| {
        !(anchored_read && line.starts_with("[lines ")
            || call.name == "bash"
                && line.starts_with("[complete output: artifact://")
                && line.ends_with(']'))
    }) {
        line.hash(&mut hash);
    }
    hash.finish()
}

/// `path#TAG` of a read result's `[path#TAG]` header line. The tag names the file content, so a
/// changed file has a different header.
fn source_header(line: &str) -> Option<&str> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let (path, tag) = inner.rsplit_once('#')?;
    (!path.is_empty() && tag.len() == 4 && tag.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(inner)
}

/// Inclusive source-line run a read result showed.
type Run = (u32, u32);

/// The line runs each `[path#TAG]` section of a read result showed. Empty when the result is not
/// made of anchored rows, or when any section shows none (e.g. a per-path error), so that such a
/// result is never replaced by a pointer.
fn shown_runs(text: &str) -> Vec<(&str, Vec<Run>)> {
    let mut sections: Vec<(&str, Vec<Run>)> = Vec::new();
    for line in text.lines() {
        if let Some(header) = source_header(line) {
            sections.push((header, Vec::new()));
            continue;
        }
        // Clipped source rows cannot cover a later complete read of the same line.
        if line.ends_with("…[truncated]") || line.ends_with("…[line truncated]") {
            continue;
        }
        let Some(number) = line
            .split_once(':')
            .and_then(|(digits, _)| digits.parse::<u32>().ok())
        else {
            continue;
        };
        let Some((_, runs)) = sections.last_mut() else {
            continue;
        };
        match runs.last_mut() {
            Some(last) if last.1.checked_add(1) == Some(number) => last.1 = number,
            _ => runs.push((number, number)),
        }
    }
    if sections.iter().any(|(_, runs)| runs.is_empty()) {
        sections.clear();
    }
    sections
}

/// Anchored rows in one file section of a `read` result above which dial suggests a structural
/// lookup first. A whole-file read past the output cap already returns an elided summary; this
/// covers the reads below the cap that still put hundreds of lines into the context.
pub(in crate::agent) const LARGE_READ_ROWS: u32 = 300;

/// A one-line `[hint]` for a successful `read` that returned at least [`LARGE_READ_ROWS`] rows of
/// one file whose type `outline` can parse. Reads that already named declarations (`:sym=`,
/// `:outline=`) get none. The hint shapes the next lookup; it never changes this result.
pub(in crate::agent) fn large_read_hint(
    call: &ToolCall,
    text: &str,
    outline: impl Fn(&str) -> bool,
) -> Option<String> {
    let args = &call.arguments;
    let requested = args["path"].as_str().into_iter().chain(
        args["paths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str()),
    );
    if requested.clone().any(|path| {
        path.contains(":sym=") || path.contains(":symbol=") || path.contains(":outline")
    }) || requested.clone().next().is_none()
    {
        return None;
    }
    let (path, rows) = shown_runs(text).into_iter().find_map(|(header, runs)| {
        let rows: u32 = runs.iter().map(|(a, b)| b - a + 1).sum();
        let (path, _) = header.rsplit_once('#')?;
        let path = serde_json::from_str::<String>(path).unwrap_or_else(|_| path.to_owned());
        (rows >= LARGE_READ_ROWS && outline(&path)).then_some((path, rows))
    })?;
    Some(format!(
        "[hint] This read returned {rows} lines of {path}. Start a file this size with `{path}:outline`, or `:sym=NAME` for one declaration, then read only the ranges you need."
    ))
}

/// Opening of the pointer that answers an identical re-read whose earlier full result is still shown.
pub(in crate::agent) const UNCHANGED_READ: &str = "[Unchanged read]";

/// Whether a withheld or replacement body is such a pointer rather than advice or the result itself.
pub(in crate::agent) fn is_unchanged_read(body: &str) -> bool {
    body.starts_with(UNCHANGED_READ)
}

/// Turn-local source rows returned by reads, edits and opt-in discovery previews.
/// Fresh rereads reuse only unchanged ranges whose earlier result remains visible in full.
#[derive(Default)]
pub(in crate::agent) struct ReturnedReads {
    seen: std::collections::HashMap<String, (u64, String)>,
    /// Line runs shown in full by earlier reads, per `path#TAG`, with the call that returned them.
    shown: std::collections::HashMap<String, Vec<(Run, String)>>,
    native: ReadReuse,
}

impl ReturnedReads {
    const MAX_FILES: usize = 256;
    const MAX_RUNS_PER_FILE: usize = 64;

    /// Freeze only coverage whose original complete result is still in this controller's context.
    pub fn reusable(&self, still_full: impl Fn(&str) -> bool) -> ReadReuse {
        ReadReuse {
            files: self
                .native
                .files
                .iter()
                .filter_map(|(key, runs)| {
                    let runs: Vec<_> = runs
                        .iter()
                        .filter(|(_, id)| still_full(id))
                        .cloned()
                        .collect();
                    (!runs.is_empty()).then(|| (key.clone(), runs))
                })
                .collect(),
        }
    }

    /// Drops cached pointers and native coverage whose tool results are no longer shown in full.
    pub fn retain_visible(&mut self, still_full: impl Fn(&str) -> bool) {
        self.seen.retain(|_, (_, id)| still_full(id));
        for runs in self.shown.values_mut() {
            runs.retain(|(_, id)| still_full(id));
        }
        self.shown.retain(|_, runs| !runs.is_empty());
        self.native.retain_visible(still_full);
    }

    /// Pointer text to send instead of `text`, or None when the full result must be sent (and is recorded).
    /// A read is also answered by a pointer when every line it shows lies inside lines an earlier
    /// read of the same unchanged file returned, and those results are still shown in full.
    pub fn pointer(
        &mut self,
        call: &ToolCall,
        text: &str,
        is_error: bool,
        still_full: impl Fn(&str) -> bool,
    ) -> Option<String> {
        if is_error {
            return None;
        }
        if is_unchanged_read(text) {
            return Some(text.to_owned());
        }
        if matches!(
            call.name.as_str(),
            "edit" | "write" | "glob" | "find" | "grep"
        ) {
            self.native.remember(call, text);
            self.remember(shown_runs(text), &call.id);
            return None;
        }
        if call.name != "read" {
            return None;
        }
        let target = ["path", "paths", "offset", "limit"]
            .map(|name| call.arguments[name].to_string())
            .join("\0");
        let key = observation_key(call, text);
        let sections = shown_runs(text);
        // `fresh: true` asks for every row again, for example after the earlier result was summarized.
        let fresh = call.arguments["fresh"].as_bool() == Some(true);
        let ids = if fresh {
            None
        } else if let Some((previous, id)) = self.seen.get(&target)
            && *previous == key
            && still_full(id)
        {
            Some(vec![id.clone()])
        } else {
            self.covering(&sections, &still_full)
        };
        if let Some(ids) = ids {
            let pointer = format!(
                "{UNCHANGED_READ} Fresh read succeeded; same rows in still-visible tool call {}. Reuse them.",
                ids.join(", ")
            );
            // Reusing a tiny range must not cost more bytes than returning that range.
            if pointer.len() < text.len() {
                return Some(pointer);
            }
        }
        if self.seen.len() >= 256 {
            self.seen.clear();
        }
        self.seen.insert(target, (key, call.id.clone()));
        self.remember(sections, &call.id);
        self.native.remember(call, text);
        None
    }

    fn remember(&mut self, sections: Vec<(&str, Vec<Run>)>, id: &str) {
        for (header, runs) in sections {
            if self.shown.len() >= Self::MAX_FILES && !self.shown.contains_key(header) {
                self.shown.clear();
            }
            let known = self.shown.entry(header.to_owned()).or_default();
            known.extend(runs.into_iter().map(|run| (run, id.to_owned())));
            if known.len() > Self::MAX_RUNS_PER_FILE {
                known.drain(..known.len() - Self::MAX_RUNS_PER_FILE);
            }
        }
    }

    /// Calls whose still-visible results together contain every line of `sections`.
    fn covering(
        &self,
        sections: &[(&str, Vec<Run>)],
        still_full: &impl Fn(&str) -> bool,
    ) -> Option<Vec<String>> {
        if sections.is_empty() {
            return None;
        }
        let mut ids: Vec<String> = Vec::new();
        for (header, runs) in sections {
            let known = self.shown.get(*header)?;
            for &(first, last) in runs {
                let mut next = first;
                loop {
                    // Adjacent/overlapping earlier reads can jointly cover this run. Choose the
                    // furthest visible end so the pointer needs as few earlier calls as possible.
                    let ((_, to), id) = known
                        .iter()
                        .filter(|((from, to), id)| *from <= next && next <= *to && still_full(id))
                        .max_by_key(|((_, to), _)| *to)?;
                    if !ids.contains(id) {
                        ids.push(id.clone());
                    }
                    if *to >= last {
                        break;
                    }
                    next = to + 1;
                }
            }
        }
        Some(ids)
    }
}

/// Visible, complete source rows, indexed by canonical display path and full raw-file SHA-256.
/// No shell results, short tags, missing rows or hidden/pruned results confer coverage.
#[derive(Clone, Default)]
pub(in crate::agent) struct ReadReuse {
    files: std::collections::BTreeMap<(String, String), Vec<(Run, String)>>,
}

impl ReadReuse {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn retain_visible(&mut self, still_full: impl Fn(&str) -> bool) {
        for runs in self.files.values_mut() {
            runs.retain(|(_, id)| still_full(id));
        }
        self.files.retain(|_, runs| !runs.is_empty());
    }

    fn remember(&mut self, call: &ToolCall, text: &str) {
        for (header, runs) in shown_runs(text) {
            let Some((path, _)) = header.rsplit_once('#') else {
                continue;
            };
            let marker = format!("[{header}]\n[Source raw SHA256:");
            let Some(revision) = text
                .split_once(&marker)
                .and_then(|(_, rest)| rest.split_once(']').map(|(digest, _)| digest))
            else {
                continue;
            };
            if revision.len() != 64 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let path = if path.starts_with('"') {
                let Ok(path) = serde_json::from_str::<String>(path) else {
                    continue;
                };
                path
            } else {
                path.to_owned()
            };
            let key = (path, revision.to_owned());
            if self.files.len() >= ReturnedReads::MAX_FILES && !self.files.contains_key(&key) {
                self.files.clear();
            }
            let known = self.files.entry(key).or_default();
            known.extend(runs.into_iter().map(|run| (run, call.id.clone())));
            if known.len() > ReturnedReads::MAX_RUNS_PER_FILE {
                known.drain(..known.len() - ReturnedReads::MAX_RUNS_PER_FILE);
            }
        }
    }

    /// Execute an already-admitted native read, replacing covered ranges with evidence links.
    /// Source hashing is still performed; a reused receipt is not a new file observation/check.
    pub fn execute(
        &self,
        workspace: &crate::tools::Workspace,
        args: &serde_json::Value,
        mut read: impl FnMut(&serde_json::Value) -> crate::runtime::tool_output::ToolOutput,
    ) -> crate::runtime::tool_output::ToolOutput {
        use crate::runtime::tool_output::ToolOutput;
        use serde_json::json;
        if self.is_empty() || args["fresh"].as_bool() == Some(true) {
            return read(args);
        }
        if args
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .is_some()
            && args.get("path").is_none_or(serde_json::Value::is_null)
            && args.get("format").is_none_or(serde_json::Value::is_null)
        {
            return self
                .execute_paths(workspace, args, &mut read)
                .unwrap_or_else(|| read(args));
        }
        let Some(selection) = workspace.native_read_selection(args) else {
            return self
                .delta(workspace, args, &mut read)
                .unwrap_or_else(|| read(args));
        };
        let Some(known) = self
            .files
            .get(&(selection.path.clone(), selection.revision.clone()))
        else {
            return self
                .delta(workspace, args, &mut read)
                .unwrap_or_else(|| read(args));
        };
        let mut missing = Vec::new();
        let mut covered = Vec::new();
        for &(first, last) in &selection.ranges {
            let mut next = first;
            while next <= last {
                if let Some(((.., end), id)) = known
                    .iter()
                    .filter(|((start, end), _)| *start <= next && next <= *end)
                    .max_by_key(|((_, end), _)| *end)
                {
                    let end = (*end).min(last);
                    // A pointer must not trap edit recovery after its source witness was evicted.
                    if !selection
                        .witnessed_ranges
                        .iter()
                        .any(|&(from, to)| from <= next && end <= to)
                    {
                        return read(args);
                    }
                    covered.push(json!({"range":[next,end],"call_id":id}));
                    next = end + 1;
                } else {
                    let end = known
                        .iter()
                        .filter(|((start, _), _)| *start > next)
                        .map(|((start, _), _)| start - 1)
                        .min()
                        .unwrap_or(last)
                        .min(last);
                    missing.push((next, end));
                    next = end + 1;
                }
            }
        }
        if covered.is_empty() {
            return read(args);
        }
        let ids = covered
            .iter()
            .filter_map(|part| part["call_id"].as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ");
        let fully_reused = missing.is_empty();
        let pointer = format!(
            "{UNCHANGED_READ} Source SHA-256 revalidated; requested rows are already in visible tool call(s) {ids}. No native read executed; reuse that evidence."
        );
        // Reusing tiny rows must not cost more than returning them. The native read always emits
        // its revision header and the requested source rows, a lower bound on its rendered size.
        let header = format!("[Source raw SHA256:{}]\n", selection.revision).len();
        if fully_reused
            && workspace
                .resolve_read_target(&selection.path)
                .ok()
                .and_then(|path| std::fs::read(path).ok())
                .is_none_or(|source| {
                    pointer.len() >= header + requested_bytes(&source, &selection.ranges)
                })
        {
            return read(args);
        }
        let mut output = if fully_reused {
            ToolOutput::ok(pointer)
        } else {
            let ranges = missing
                .iter()
                .map(|(first, last)| format!("{first}-{last}"))
                .collect::<Vec<_>>()
                .join(",");
            let mut narrowed = args.clone();
            let object = narrowed.as_object_mut().expect("validated read arguments");
            object.remove("offset");
            object.remove("limit");
            object.insert("path".into(), json!(format!("{}:{ranges}", selection.path)));
            // Literal colon-containing filenames win over selectors in the native parser.
            // Never let narrowing accidentally change the requested target.
            if workspace
                .resolve_read_target(narrowed["path"].as_str().expect("read path"))
                .ok()
                != workspace.resolve_read_target(&selection.path).ok()
            {
                return read(args);
            }
            let mut output = read(&narrowed);
            let expected = format!("[Source raw SHA256:{}]", selection.revision);
            if !output.is_error && output.text.lines().nth(1) != Some(expected.as_str()) {
                // Only the native section header authenticates its revision, never source content.
                // An external edit raced the narrowed read. Deliver the original selection of
                // the new version rather than mixing old coverage with its missing rows.
                return read(args);
            }
            if output.is_error {
                return output;
            }
            let reused = covered
                .iter()
                .map(|part| {
                    format!(
                        "{}-{} (call {})",
                        part["range"][0],
                        part["range"][1],
                        part["call_id"].as_str().expect("coverage call ID")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            output.text.push_str(&format!("\n[Read reuse] Requested ranges {reused} are already visible; only missing ranges {ranges} were sent to the native read renderer."));
            output
        };
        output.data = Some(json!({"read_reuse":{
            "path":selection.path,"source_sha256":selection.revision,
            "covered":covered,"executed_ranges":missing,"fully_reused":fully_reused
        }}));
        output
    }

    /// A read of a file that changed after a still-visible earlier read: send only the changed
    /// lines, with a note mapping the unchanged ones onto that earlier call. `None` falls back to
    /// an ordinary read (no visible earlier revision, unmappable change, or a racing edit).
    fn delta(
        &self,
        workspace: &crate::tools::Workspace,
        args: &serde_json::Value,
        read: &mut impl FnMut(&serde_json::Value) -> crate::runtime::tool_output::ToolOutput,
    ) -> Option<crate::runtime::tool_output::ToolOutput> {
        use crate::runtime::tool_output::ToolOutput;
        use serde_json::json;
        // The selection's canonical path; an unqualified whole-file read is probed as its first page.
        let mut probe = args.clone();
        if args.get("offset").is_none_or(serde_json::Value::is_null)
            && args.get("limit").is_none_or(serde_json::Value::is_null)
        {
            probe["limit"] = json!(2000);
        }
        let target = workspace.native_read_selection(&probe)?.path;
        let (revision, plan) = self
            .files
            .range((target.clone(), String::new())..)
            .take_while(|((path, _), _)| *path == target)
            .find_map(|((_, revision), runs)| {
                workspace
                    .read_delta_plan(args, revision, runs)
                    .map(|plan| (revision.clone(), plan))
            })?;
        let note = plan.note(&revision);
        let covered: Vec<_> = plan
            .covered
            .iter()
            .map(|run| {
                json!({"range":[run.current.0,run.current.1],
                    "earlier_range":[run.earlier.0,run.earlier.1],"call_id":run.call_id})
            })
            .collect();
        let mut output = if plan.missing.is_empty() {
            ToolOutput::ok(note)
        } else {
            let ranges = plan
                .missing
                .iter()
                .map(|(first, last)| format!("{first}-{last}"))
                .collect::<Vec<_>>()
                .join(",");
            let mut narrowed = args.clone();
            let object = narrowed.as_object_mut()?;
            object.remove("offset");
            object.remove("limit");
            let narrowed_path = format!("{target}:{ranges}");
            if workspace.resolve_read_target(&narrowed_path).ok()
                != workspace.resolve_read_target(&target).ok()
            {
                return None;
            }
            object.insert("path".into(), json!(narrowed_path));
            let mut output = read(&narrowed);
            let expected = format!("[Source raw SHA256:{}]", plan.revision);
            if output.is_error || output.text.lines().nth(1) != Some(expected.as_str()) {
                // The file changed again between planning and reading.
                return None;
            }
            output.text.push_str(&format!("\n{note}"));
            output
        };
        output.data = Some(json!({"read_reuse":{
            "path":plan.path,"source_sha256":plan.revision,"earlier_sha256":revision,
            "covered":covered,"executed_ranges":plan.missing,
            "fully_reused":plan.missing.is_empty(),"delta":true
        }}));
        Some(output)
    }

    /// Reuse or narrow each target of a batched `read(paths: [...])` call. Targets without visible
    /// coverage pass through unchanged; racing edits or budget omissions fall back to `read(args)`.
    fn execute_paths(
        &self,
        workspace: &crate::tools::Workspace,
        args: &serde_json::Value,
        read: &mut impl FnMut(&serde_json::Value) -> crate::runtime::tool_output::ToolOutput,
    ) -> Option<crate::runtime::tool_output::ToolOutput> {
        use crate::runtime::tool_output::ToolOutput;
        use serde_json::{Value, json};
        let targets: Vec<&str> = args["paths"]
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?;
        if targets.is_empty() || targets.len() > 16 {
            return None;
        }
        let has_outer_window = args.get("offset").is_some_and(|v| !v.is_null())
            || args.get("limit").is_some_and(|v| !v.is_null());
        let mut base_single = args.clone();
        base_single.as_object_mut()?.remove("paths");

        enum TargetPlan {
            Passthrough,
            Unchanged {
                path: String,
                revision: String,
                ranges: Vec<(u32, u32)>,
                covered: Vec<Value>,
                missing: Vec<(u32, u32)>,
                narrowed_path: Option<String>,
            },
            Delta {
                path: String,
                revision: String,
                covered: Vec<Value>,
                missing: Vec<(u32, u32)>,
                note: String,
                narrowed_path: Option<String>,
            },
        }

        let mut plans = Vec::with_capacity(targets.len());
        let mut any_reused = false;
        let mut any_passthrough = false;
        let mut any_narrowed = false;
        let mut any_delta = false;

        for &target in &targets {
            let mut entry_args = base_single.clone();
            entry_args["path"] = json!(target);
            let plan = if let Some(selection) = workspace.native_read_selection(&entry_args)
                && let Some(known) = self
                    .files
                    .get(&(selection.path.clone(), selection.revision.clone()))
            {
                let mut missing = Vec::new();
                let mut covered = Vec::new();
                let mut witness_missing = false;
                for &(first, last) in &selection.ranges {
                    let mut next = first;
                    while next <= last {
                        if let Some(((.., end), id)) = known
                            .iter()
                            .filter(|((start, end), _)| *start <= next && next <= *end)
                            .max_by_key(|((_, end), _)| *end)
                        {
                            let end = (*end).min(last);
                            if !selection
                                .witnessed_ranges
                                .iter()
                                .any(|&(from, to)| from <= next && end <= to)
                            {
                                witness_missing = true;
                                break;
                            }
                            covered.push(json!({"path": selection.path, "range": [next, end], "call_id": id}));
                            next = end + 1;
                        } else {
                            let end = known
                                .iter()
                                .filter(|((start, _), _)| *start > next)
                                .map(|((start, _), _)| start - 1)
                                .min()
                                .unwrap_or(last)
                                .min(last);
                            missing.push((next, end));
                            next = end + 1;
                        }
                    }
                    if witness_missing {
                        break;
                    }
                }
                if witness_missing || covered.is_empty() {
                    TargetPlan::Passthrough
                } else if missing.is_empty() {
                    TargetPlan::Unchanged {
                        path: selection.path,
                        revision: selection.revision,
                        ranges: selection.ranges,
                        covered,
                        missing,
                        narrowed_path: None,
                    }
                } else {
                    let ranges_spec = missing
                        .iter()
                        .map(|(first, last)| format!("{first}-{last}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    let narrowed_path = format!("{}:{ranges_spec}", selection.path);
                    if workspace.resolve_read_target(&narrowed_path).ok()
                        == workspace.resolve_read_target(&selection.path).ok()
                    {
                        TargetPlan::Unchanged {
                            path: selection.path,
                            revision: selection.revision,
                            ranges: selection.ranges,
                            covered,
                            missing,
                            narrowed_path: Some(narrowed_path),
                        }
                    } else {
                        TargetPlan::Passthrough
                    }
                }
            } else {
                // Check for a delta against a visible earlier revision of this target.
                let mut probe = entry_args.clone();
                if entry_args.get("offset").is_none_or(Value::is_null)
                    && entry_args.get("limit").is_none_or(Value::is_null)
                {
                    probe["limit"] = json!(2000);
                }
                let delta_plan = workspace
                    .native_read_selection(&probe)
                    .map(|sel| sel.path)
                    .and_then(|target_path| {
                        let (earlier_revision, plan) = self
                            .files
                            .range((target_path.clone(), String::new())..)
                            .take_while(|((path, _), _)| *path == target_path)
                            .find_map(|((_, rev), runs)| {
                                workspace
                                    .read_delta_plan(&entry_args, rev, runs)
                                    .map(|plan| (rev.clone(), plan))
                            })?;
                        let note = plan.note(&earlier_revision);
                        let covered: Vec<_> = plan
                            .covered
                            .iter()
                            .map(|run| {
                                json!({
                                    "path": plan.path,
                                    "range": [run.current.0, run.current.1],
                                    "earlier_range": [run.earlier.0, run.earlier.1],
                                    "call_id": run.call_id,
                                })
                            })
                            .collect();
                        let narrowed_path = if plan.missing.is_empty() {
                            None
                        } else {
                            let ranges_spec = plan
                                .missing
                                .iter()
                                .map(|(first, last)| format!("{first}-{last}"))
                                .collect::<Vec<_>>()
                                .join(",");
                            let narrowed_path = format!("{target_path}:{ranges_spec}");
                            if workspace.resolve_read_target(&narrowed_path).ok()
                                != workspace.resolve_read_target(&target_path).ok()
                            {
                                return None;
                            }
                            Some(narrowed_path)
                        };
                        Some(TargetPlan::Delta {
                            path: plan.path,
                            revision: plan.revision,
                            covered,
                            missing: plan.missing,
                            note,
                            narrowed_path,
                        })
                    });
                delta_plan.unwrap_or(TargetPlan::Passthrough)
            };
            match &plan {
                TargetPlan::Passthrough => any_passthrough = true,
                TargetPlan::Unchanged { narrowed_path, .. } => {
                    any_reused = true;
                    any_narrowed |= narrowed_path.is_some();
                }
                TargetPlan::Delta { narrowed_path, .. } => {
                    any_reused = true;
                    any_delta = true;
                    any_narrowed |= narrowed_path.is_some();
                }
            }
            plans.push(plan);
        }

        if !any_reused || (has_outer_window && any_passthrough && any_narrowed) {
            return None;
        }

        let mut all_covered = Vec::new();
        let mut all_missing = Vec::new();
        let mut all_ids = std::collections::BTreeSet::new();
        let mut effective_targets: Vec<String> = Vec::new();
        let mut expected_revisions: Vec<(String, String)> = Vec::new();
        let mut reuse_notes: Vec<String> = Vec::new();
        let mut delta_notes: Vec<String> = Vec::new();
        let unchanged_only = !any_passthrough && !any_delta && !any_narrowed;
        let mut total_unchanged_bytes: usize = 0;

        for (target, plan) in targets.iter().zip(plans) {
            match plan {
                TargetPlan::Passthrough => {
                    effective_targets.push((*target).to_owned());
                }
                TargetPlan::Unchanged {
                    path,
                    revision,
                    ranges,
                    covered,
                    missing,
                    narrowed_path,
                } => {
                    if unchanged_only {
                        let header = format!("[Source raw SHA256:{revision}]\n").len();
                        let source = workspace
                            .resolve_read_target(&path)
                            .ok()
                            .and_then(|resolved| std::fs::read(resolved).ok())?;
                        total_unchanged_bytes = total_unchanged_bytes
                            .saturating_add(header + requested_bytes(&source, &ranges));
                    }
                    let summary = covered
                        .iter()
                        .map(|part| {
                            let id = part["call_id"].as_str().unwrap_or("");
                            all_ids.insert(id.to_owned());
                            format!(
                                "{path}:{}-{} (call {id})",
                                part["range"][0], part["range"][1]
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    all_covered.extend(covered);
                    if let Some(narrowed) = narrowed_path {
                        let missing_spec = missing
                            .iter()
                            .map(|(first, last)| format!("{first}-{last}"))
                            .collect::<Vec<_>>()
                            .join(",");
                        reuse_notes.push(format!(
                            "{summary} already visible; read only {path}:{missing_spec}"
                        ));
                        all_missing.extend(missing);
                        effective_targets.push(narrowed);
                        expected_revisions.push((path, revision));
                    } else {
                        reuse_notes.push(format!("{summary} already visible"));
                    }
                }
                TargetPlan::Delta {
                    path,
                    revision,
                    covered,
                    missing,
                    note,
                    narrowed_path,
                } => {
                    for part in &covered {
                        if let Some(id) = part["call_id"].as_str() {
                            all_ids.insert(id.to_owned());
                        }
                    }
                    all_covered.extend(covered);
                    all_missing.extend(missing);
                    delta_notes.push(note);
                    if let Some(narrowed) = narrowed_path {
                        effective_targets.push(narrowed);
                        expected_revisions.push((path, revision));
                    }
                }
            }
        }

        if effective_targets.is_empty() {
            if unchanged_only {
                let ids = all_ids.into_iter().collect::<Vec<_>>().join(", ");
                let pointer = format!(
                    "{UNCHANGED_READ} Source SHA-256 revalidated; requested rows are already in visible tool call(s) {ids}. No native read executed; reuse that evidence."
                );
                if pointer.len() >= total_unchanged_bytes {
                    return None;
                }
                let mut output = ToolOutput::ok(pointer);
                output.data = Some(json!({"read_reuse": {
                    "covered": all_covered,
                    "executed_ranges": all_missing,
                    "fully_reused": true,
                }}));
                return Some(output);
            }
            let mut sections = Vec::new();
            if !reuse_notes.is_empty() {
                sections.push(format!("[Read reuse] {}.", reuse_notes.join("; ")));
            }
            sections.extend(delta_notes);
            let mut output = ToolOutput::ok(sections.join("\n"));
            output.data = Some(json!({"read_reuse": {
                "covered": all_covered,
                "executed_ranges": all_missing,
                "fully_reused": true,
                "delta": any_delta,
            }}));
            return Some(output);
        }

        let mut narrowed_args = args.clone();
        let object = narrowed_args.as_object_mut()?;
        if any_narrowed {
            object.remove("offset");
            object.remove("limit");
        }
        object.insert("paths".into(), json!(effective_targets));
        let mut output = read(&narrowed_args);
        if output.is_error {
            return Some(output);
        }
        let revisions = section_revisions(&output.text);
        for (path, expected) in &expected_revisions {
            if revisions.get(path.as_str()).copied() != Some(expected.as_str()) {
                // An external edit raced or the shared budget omitted a narrowed section.
                return None;
            }
        }
        if !reuse_notes.is_empty() {
            output
                .text
                .push_str(&format!("\n[Read reuse] {}.", reuse_notes.join("; ")));
        }
        for note in delta_notes {
            output.text.push('\n');
            output.text.push_str(&note);
        }
        let reuse_meta = json!({
            "covered": all_covered,
            "executed_ranges": all_missing,
            "fully_reused": false,
            "delta": any_delta,
        });
        match output.data.as_mut().and_then(Value::as_object_mut) {
            Some(map) => {
                map.insert("read_reuse".into(), reuse_meta);
            }
            None => {
                output.data = Some(json!({"read_reuse": reuse_meta}));
            }
        }
        Some(output)
    }
}

/// Authenticated `[path#TAG]` -> `[Source raw SHA256:...]` pairs parsed from native section headers.
/// Source rows always start with a line number, so source text cannot spoof column-0 headers.
fn section_revisions(text: &str) -> std::collections::HashMap<String, &str> {
    let mut out = std::collections::HashMap::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(header) = source_header(line) else {
            continue;
        };
        let Some((raw_path, _)) = header.rsplit_once('#') else {
            continue;
        };
        let Some(next) = lines.peek().copied() else {
            break;
        };
        let Some(rev) = next
            .strip_prefix("[Source raw SHA256:")
            .and_then(|rest| rest.strip_suffix(']'))
            .filter(|rev| rev.len() == 64 && rev.bytes().all(|b| b.is_ascii_hexdigit()))
        else {
            continue;
        };
        lines.next();
        let path = if raw_path.starts_with('"') {
            let Ok(decoded) = serde_json::from_str::<String>(raw_path) else {
                continue;
            };
            decoded
        } else {
            raw_path.to_owned()
        };
        out.insert(path, rev);
    }
    out
}

/// Bytes of the requested 1-based inclusive line ranges in `source`.
fn requested_bytes(source: &[u8], ranges: &[(u32, u32)]) -> usize {
    source
        .split_inclusive(|&byte| byte == b'\n')
        .enumerate()
        .filter(|(index, _)| {
            let line = *index as u32 + 1;
            ranges
                .iter()
                .any(|&(first, last)| first <= line && line <= last)
        })
        .map(|(_, line)| line.len())
        .sum()
}

#[cfg(test)]
mod tests;
