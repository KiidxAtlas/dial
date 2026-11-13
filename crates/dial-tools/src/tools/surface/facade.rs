//! One model-facing tool composed of existing tool owners. A facade adds no behavior: each call is
//! routed to exactly one member, translated back to that member's own arguments, validated against
//! the member's own schema and executed by the member, so every per-tool bound, approval rule and
//! scheduling class still applies. Consolidation therefore shrinks the catalog without removing a
//! capability or a safety check.
use crate::runtime::tool_output::ToolOutput;
use crate::tools::integrations::ext::{
    Concurrency, JobsView, Tool, ToolCallback, ToolContext, ToolEvent,
};
use crate::tools::surface::catalog::Readiness;
use crate::tools::{Workspace, tool_schema};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// How a call selects a member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// `args[field] == value` selects the member; `field` is removed before delegating.
    Select { field: &'static str, value: String },
    /// `args.action == merged` selects the member; `action` becomes `recorded` (or is removed).
    Action {
        merged: &'static str,
        recorded: Option<&'static str>,
    },
}

#[derive(Clone)]
pub struct Member {
    pub route: Route,
    pub tool: Arc<dyn Tool>,
}

impl Member {
    pub fn select(field: &'static str, value: impl Into<String>, tool: Arc<dyn Tool>) -> Self {
        Self {
            route: Route::Select {
                field,
                value: value.into(),
            },
            tool,
        }
    }
    pub fn action(
        merged: &'static str,
        recorded: Option<&'static str>,
        tool: Arc<dyn Tool>,
    ) -> Self {
        Self {
            route: Route::Action { merged, recorded },
            tool,
        }
    }
}

/// A consolidated tool. `native` handles calls no member route matches (the successor's own
/// actions); without it the selector is required.
#[derive(Clone)]
pub struct Facade {
    name: String,
    description: String,
    selector_note: &'static str,
    native: Option<Arc<dyn Tool>>,
    members: Vec<Member>,
}

impl Facade {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        selector_note: &'static str,
        native: Option<Arc<dyn Tool>>,
        members: Vec<Member>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            selector_note,
            native,
            members,
        }
    }

    /// Every distinct owner once, native first (several action routes can share one owner).
    fn owners(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        let mut seen: Vec<&Arc<dyn Tool>> = Vec::new();
        self.native
            .iter()
            .chain(self.members.iter().map(|m| &m.tool))
            .filter(move |tool| {
                if seen.iter().any(|prior| Arc::ptr_eq(prior, tool)) {
                    false
                } else {
                    seen.push(tool);
                    true
                }
            })
    }

    /// The owner of a call and the owner's own arguments. `None` when nothing matches.
    pub fn resolve(&self, args: &Value) -> Option<(&Arc<dyn Tool>, Value)> {
        for member in &self.members {
            match &member.route {
                Route::Select { field, value } => {
                    if args.get(*field).and_then(Value::as_str) == Some(value.as_str()) {
                        let mut own = args.clone();
                        own.as_object_mut()?.remove(*field);
                        return Some((&member.tool, own));
                    }
                }
                Route::Action { merged, recorded } => {
                    if args.get("action").and_then(Value::as_str) == Some(*merged) {
                        let mut own = args.clone();
                        let object = own.as_object_mut()?;
                        match recorded {
                            Some(recorded) => {
                                object.insert("action".into(), json!(recorded));
                            }
                            None => {
                                object.remove("action");
                            }
                        }
                        return Some((&member.tool, own));
                    }
                }
            }
        }
        let selected = self.members.iter().any(|member| match &member.route {
            Route::Select { field, .. } => args.get(*field).is_some(),
            Route::Action { .. } => false,
        });
        if selected {
            return None;
        }
        self.native.as_ref().map(|tool| (tool, args.clone()))
    }

    fn unresolved(&self, args: &Value) -> ToolOutput {
        let choices: Vec<String> = self
            .members
            .iter()
            .map(|member| match &member.route {
                Route::Select { field, value } => format!("{field}={value}"),
                Route::Action { merged, .. } => format!("action={merged}"),
            })
            .collect();
        ToolOutput::err(format!(
            "{}: no operation matches {}; choose one of: {}{}",
            self.name,
            args,
            choices.join(", "),
            if self.native.is_some() {
                " (or one of the tool's own actions)"
            } else {
                ""
            }
        ))
    }

    /// The member's own validation: its exact bounds, required fields and strictness. A refusal
    /// carries the member's complete description, which the compact facade schema summarizes, and
    /// names the selector whose member does offer a refused action.
    fn validated(&self, owner: &Arc<dyn Tool>, own: &Value) -> Result<(), String> {
        let schema = owner.schema();
        crate::tools::surface::catalog::validate_arguments(&schema, own).map_err(|error| {
            format!(
                "{} ({}): {error}{}{}\nContract: {}\nParameters: {}",
                self.name,
                owner.name(),
                self.action_elsewhere(owner, own),
                self.property_elsewhere(owner, own),
                schema["function"]["description"]
                    .as_str()
                    .unwrap_or_default(),
                schema["function"]["parameters"]
            )
        })
    }

    /// `(recorded, merged)` action names of every action route to `tool`.
    fn action_names(&self, tool: &Arc<dyn Tool>) -> Vec<(&'static str, &'static str)> {
        self.members
            .iter()
            .filter(|member| Arc::ptr_eq(&member.tool, tool))
            .filter_map(|member| match &member.route {
                Route::Action {
                    merged,
                    recorded: Some(recorded),
                } => Some((*recorded, *merged)),
                _ => None,
            })
            .collect()
    }

    /// `"\nafter_secs belongs to action=watch"` when `own` carries arguments its owner does not
    /// declare but another member does, e.g. a monitor argument sent with `bg`'s own `start`.
    /// Among a member's action routes, the one recording the call's action is named first.
    fn property_elsewhere(&self, owner: &Arc<dyn Tool>, own: &Value) -> String {
        let declared = |tool: &Arc<dyn Tool>, key: &str| {
            tool.schema()
                .pointer("/function/parameters/properties")
                .and_then(Value::as_object)
                .is_some_and(|properties| properties.contains_key(key))
        };
        let Some(arguments) = own.as_object() else {
            return String::new();
        };
        let stray: Vec<&str> = arguments
            .iter()
            .filter(|(key, value)| !value.is_null() && !declared(owner, key))
            .map(|(key, _)| key.as_str())
            .collect();
        if stray.is_empty() {
            return String::new();
        }
        let action = own.get("action").and_then(Value::as_str);
        let mut routes: Vec<String> = Vec::new();
        for member in &self.members {
            if Arc::ptr_eq(&member.tool, owner)
                || !stray.iter().all(|key| declared(&member.tool, key))
            {
                continue;
            }
            let route = match &member.route {
                Route::Select { field, value } => format!("{field}={value}"),
                Route::Action { merged, recorded } => {
                    let names = self.action_names(&member.tool);
                    let same = |name: &&str| Some(*name) == action;
                    if names.iter().any(|(r, _)| same(r)) && !recorded.as_ref().is_some_and(same) {
                        continue;
                    }
                    format!("action={merged}")
                }
            };
            if !routes.contains(&route) {
                routes.push(route);
            }
        }
        if routes.is_empty() {
            return String::new();
        }
        format!(
            "\n{} belongs to {}; resend with that route",
            stray.join(", "),
            routes.join(" or ")
        )
    }

    /// `"\naction=show belongs to source=changes"` when the selected member lacks `own`'s action
    /// but another selector member offers it; empty otherwise.
    fn action_elsewhere(&self, owner: &Arc<dyn Tool>, own: &Value) -> String {
        let Some(action) = own.get("action").and_then(Value::as_str) else {
            return String::new();
        };
        let offers = |tool: &Arc<dyn Tool>| {
            tool.schema()
                .pointer("/function/parameters/properties/action/enum")
                .and_then(Value::as_array)
                .is_some_and(|values| values.iter().any(|value| value == action))
        };
        if offers(owner) {
            return String::new();
        }
        let selectors: Vec<String> = self
            .members
            .iter()
            .filter(|member| !Arc::ptr_eq(&member.tool, owner) && offers(&member.tool))
            .filter_map(|member| match &member.route {
                Route::Select { field, value } => Some(format!("{field}={value}")),
                Route::Action { .. } => None,
            })
            .collect();
        if selectors.is_empty() {
            String::new()
        } else {
            format!(
                "\naction={action} belongs to {}; resend with that selector",
                selectors.join(" or ")
            )
        }
    }

    fn rebuilt(
        &self,
        fork: impl Fn(&Arc<dyn Tool>) -> Option<Arc<dyn Tool>>,
    ) -> Option<Arc<dyn Tool>> {
        let mut changed = false;
        let mut copies: Vec<(Arc<dyn Tool>, Arc<dyn Tool>)> = Vec::new();
        let mut copy = |tool: &Arc<dyn Tool>| {
            if let Some((_, copied)) = copies
                .iter()
                .find(|(original, _)| Arc::ptr_eq(original, tool))
            {
                return Arc::clone(copied);
            }
            let copied = match fork(tool) {
                Some(forked) => {
                    changed = true;
                    forked
                }
                None => Arc::clone(tool),
            };
            copies.push((Arc::clone(tool), Arc::clone(&copied)));
            copied
        };
        let native = self.native.as_ref().map(&mut copy);
        let members = self
            .members
            .iter()
            .map(|member| Member {
                route: member.route.clone(),
                tool: copy(&member.tool),
            })
            .collect();
        changed.then(|| {
            Arc::new(Self {
                name: self.name.clone(),
                description: self.description.clone(),
                selector_note: self.selector_note,
                native,
                members,
            }) as Arc<dyn Tool>
        })
    }
}

/// Widen `existing` so it also accepts every value `other` accepts: enum values are united, and
/// differing bounds are dropped (each owner still enforces its own on call).
pub fn widen(existing: &mut Value, other: &Value) {
    if let (Some(left), Some(right)) = (
        existing.get("enum").and_then(Value::as_array).cloned(),
        other.get("enum").and_then(Value::as_array),
    ) {
        let mut values = left;
        for value in right {
            if !values.contains(value) {
                values.push(value.clone());
            }
        }
        existing["enum"] = Value::Array(values);
        return;
    }
    let unbounded = |value: &Value| {
        let mut value = value.clone();
        if let Some(object) = value.as_object_mut() {
            for bound in [
                "minItems",
                "maxItems",
                "minLength",
                "maxLength",
                "minimum",
                "maximum",
            ] {
                object.remove(bound);
            }
        }
        value
    };
    let (left, right) = (unbounded(existing), unbounded(other));
    if left == right {
        *existing = left;
        return;
    }
    let kind = existing
        .get("type")
        .filter(|kind| other.get("type") == Some(*kind))
        .cloned();
    let description = existing
        .get("description")
        .or_else(|| other.get("description"))
        .cloned();
    let mut widened = Map::new();
    if let Some(kind) = kind {
        widened.insert("type".into(), kind);
    }
    if let Some(description) = description {
        widened.insert("description".into(), description);
    }
    *existing = Value::Object(widened);
}

/// Add `added`'s properties to `properties`, skipping `skip`, widening shared names.
fn merge_properties(properties: &mut Map<String, Value>, added: &Value, skip: &[&str]) {
    let Some(added) = added
        .pointer("/function/parameters/properties")
        .and_then(Value::as_object)
    else {
        return;
    };
    for (name, property) in added {
        if skip.contains(&name.as_str()) {
            continue;
        }
        match properties.get_mut(name) {
            None => {
                properties.insert(name.clone(), property.clone());
            }
            Some(existing) if existing != property => widen(existing, property),
            Some(_) => {}
        }
    }
}

/// Rewrite an action-routed member's description scopes, the text before the first `:` such as
/// `start:` or `start (job/run):`, to the facade's merged action names. Without this, a member
/// property documented as `start: ...` reads as belonging to the successor's own `start` action.
fn rename_scopes(schema: &mut Value, names: &[(&str, &str)]) {
    let Some(properties) = schema
        .pointer_mut("/function/parameters/properties")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for property in properties.values_mut() {
        let Some(description) = property.get("description").and_then(Value::as_str) else {
            continue;
        };
        let Some(colon) = description.find(':').filter(|colon| *colon <= 40) else {
            continue;
        };
        let (scope, rest) = description.split_at(colon);
        let mut renamed = String::with_capacity(description.len() + 8);
        let mut word = String::new();
        let flush = |word: &mut String, out: &mut String| {
            let name = names
                .iter()
                .find(|(recorded, _)| recorded == word)
                .map_or(word.as_str(), |(_, merged)| merged);
            out.push_str(name);
            word.clear();
        };
        for c in scope.chars() {
            if c.is_ascii_alphanumeric() || c == '_' {
                word.push(c);
            } else {
                flush(&mut word, &mut renamed);
                renamed.push(c);
            }
        }
        flush(&mut word, &mut renamed);
        renamed.push_str(rest);
        property["description"] = json!(renamed);
    }
}

impl Tool for Facade {
    fn name(&self) -> &str {
        &self.name
    }

    /// The native schema (or an empty one), the selector, every merged action and the union of
    /// member properties. Members' own required fields are checked per call.
    fn schema(&self) -> Value {
        let mut schema = match &self.native {
            Some(native) => {
                let mut schema = native.schema();
                schema["function"]["description"] = json!(self.description);
                schema
            }
            None => tool_schema(&self.name, &self.description, json!({}), &[]),
        };
        let mut properties = schema
            .pointer("/function/parameters/properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut selectors: Vec<(&'static str, Vec<String>)> = Vec::new();
        let mut actions: Vec<&'static str> = Vec::new();
        for member in &self.members {
            match &member.route {
                Route::Select { field, value } => {
                    match selectors.iter_mut().find(|(name, _)| name == field) {
                        Some((_, values)) => values.push(value.clone()),
                        None => selectors.push((field, vec![value.clone()])),
                    }
                }
                Route::Action { merged, .. } => actions.push(merged),
            }
        }
        // Shared names (native ones included) are widened so every member's own call shape passes
        // the offered schema; the member's schema then enforces its exact contract per call. An
        // action-routed member's recorded actions stay hidden behind its merged action names.
        let selector_fields: Vec<&str> = selectors.iter().map(|(field, _)| *field).collect();
        for member in &self.members {
            let mut skip = selector_fields.clone();
            let mut owned = member.tool.schema();
            if matches!(member.route, Route::Action { .. }) {
                skip.push("action");
                rename_scopes(&mut owned, &self.action_names(&member.tool));
            }
            merge_properties(&mut properties, &owned, &skip);
        }
        if !actions.is_empty() {
            let action = properties
                .entry("action")
                .or_insert_with(|| json!({"type":"string","enum":[]}));
            if let Some(values) = action.get_mut("enum").and_then(Value::as_array_mut) {
                values.extend(actions.iter().map(|merged| json!(merged)));
            }
        }
        let mut required: Vec<Value> = schema
            .pointer("/function/parameters/required")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // Only requirements shared by every route can be required globally. For example GitHub
        // uses `op`, not local Git's `action`; the selected owner checks its own requirements.
        required.retain(|field| {
            self.members.iter().all(|member| {
                let schema = member.tool.schema();
                let required = schema
                    .pointer("/function/parameters/required")
                    .and_then(Value::as_array);
                match &member.route {
                    Route::Action { .. } if field == "action" => true,
                    _ => required.is_some_and(|required| required.contains(field)),
                }
            })
        });
        for (field, values) in selectors {
            properties.insert(
                field.to_owned(),
                json!({"type":"string","enum":values,"description":self.selector_note}),
            );
            if self.native.is_none() && !required.contains(&json!(field)) {
                required.push(json!(field));
            }
        }
        schema["function"]["parameters"]["properties"] = Value::Object(properties);
        schema["function"]["parameters"]["required"] = Value::Array(required);
        schema
    }

    fn readiness(&self, config: &crate::config::types::Config) -> Readiness {
        let states: Vec<Readiness> = self.owners().map(|tool| tool.readiness(config)).collect();
        if states.contains(&Readiness::Ready) {
            Readiness::Ready
        } else if states.len() == 1 {
            states.into_iter().next().unwrap_or_default()
        } else {
            Readiness::Unknown
        }
    }

    fn resources(
        &self,
        workspace: &Workspace,
        args: &Value,
    ) -> Option<crate::tools::surface::resources::Access> {
        let (owner, own) = self.resolve(args)?;
        owner.resources(workspace, &own)
    }

    fn render_call(&self, args: &Value) -> crate::tools::surface::presentation::Presentation {
        match self.resolve(args) {
            Some((owner, own)) => owner.render_call(&own),
            None => crate::tools::surface::presentation::render_call(&self.name, args),
        }
    }
    fn render_result(
        &self,
        args: &Value,
        output: &ToolOutput,
    ) -> crate::tools::surface::presentation::Presentation {
        match self.resolve(args) {
            Some((owner, own)) => owner.render_result(&own, output),
            None => crate::tools::surface::presentation::render_result(&self.name, args, output),
        }
    }
    fn outcome(&self, args: &Value, output: &ToolOutput) -> crate::runtime::Phase {
        match self.resolve(args) {
            Some((owner, own)) => owner.outcome(&own, output),
            None => crate::tools::surface::outcome::result_phase(&self.name, args, output),
        }
    }

    /// Unroutable calls are refused before execution; approval stays conservative for them.
    fn needs_approval(&self, args: &Value) -> bool {
        self.resolve(args)
            .is_none_or(|(owner, own)| owner.needs_approval(&own))
    }
    fn read_only_hint(&self) -> bool {
        self.owners().all(|tool| tool.read_only_hint())
    }
    fn may_modify_workspace(&self, args: &Value) -> bool {
        self.resolve(args)
            .is_none_or(|(owner, own)| owner.may_modify_workspace(&own))
    }
    fn supports_background(&self, args: &Value) -> bool {
        self.resolve(args)
            .is_some_and(|(owner, own)| owner.supports_background(&own))
    }
    fn fork_background(&self, cancel: Arc<AtomicBool>) -> Option<Arc<dyn Tool>> {
        self.rebuilt(|tool| tool.fork_background(Arc::clone(&cancel)))
    }
    fn manages_workspace_serialization(&self) -> bool {
        self.owners()
            .any(|tool| tool.manages_workspace_serialization())
    }
    fn concurrency(&self, args: &Value) -> Concurrency {
        self.resolve(args)
            .map_or(Concurrency::Exclusive, |(owner, own)| {
                owner.concurrency(&own)
            })
    }
    fn interruptible(&self, args: &Value) -> bool {
        self.resolve(args)
            .is_some_and(|(owner, own)| owner.interruptible(&own))
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_streaming(ctx, args, &|_| {}, &AtomicBool::new(false))
    }
    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        let Some((owner, own)) = self.resolve(args) else {
            return self.unresolved(args);
        };
        if let Err(refusal) = self.validated(owner, &own) {
            return ToolOutput::err(refusal);
        }
        owner.call_streaming(ctx, &own, on_event, cancel)
    }
    fn supports_tool_dispatch(&self) -> bool {
        self.owners().any(|tool| tool.supports_tool_dispatch())
    }
    fn call_with_dispatch(
        &self,
        ctx: &ToolContext,
        args: &Value,
        on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
        adopt: &dyn Fn(crate::tools::exec::background::AdoptedJob) -> Result<String, String>,
        dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
    ) -> ToolOutput {
        let Some((owner, own)) = self.resolve(args) else {
            return self.unresolved(args);
        };
        if let Err(refusal) = self.validated(owner, &own) {
            return ToolOutput::err(refusal);
        }
        owner.call_with_dispatch(ctx, &own, on_event, cancel, adopt, dispatch)
    }

    fn drain_notifications(&self) -> Vec<ToolEvent> {
        self.owners()
            .flat_map(|tool| tool.drain_notifications())
            .collect()
    }
    fn has_monitors(&self) -> bool {
        self.owners().any(|tool| tool.has_monitors())
    }
    fn poll_monitors(&self) -> Vec<String> {
        self.owners()
            .flat_map(|tool| tool.poll_monitors())
            .collect()
    }
    fn stop_monitors(&self) {
        self.owners().for_each(|tool| tool.stop_monitors());
    }
    fn has_live_work(&self) -> bool {
        self.owners().any(|tool| tool.has_live_work())
    }
    fn awaited_work(&self) -> Vec<String> {
        self.owners().flat_map(|tool| tool.awaited_work()).collect()
    }
    fn session_work(&self) -> Vec<String> {
        self.owners().flat_map(|tool| tool.session_work()).collect()
    }
    fn cancel_operation(&self) {
        self.owners().for_each(|tool| tool.cancel_operation());
    }
    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        self.rebuilt(|tool| tool.fork_session())
    }
    fn serves_root(&self, root: &std::path::Path) -> bool {
        self.owners().any(|tool| tool.serves_root(root))
    }
    fn attach_user_pty(&self, name: &str, bytes: &[u8]) -> Result<usize, String> {
        first_ok(self.owners().map(|tool| tool.attach_user_pty(name, bytes)))
    }
    /// Every owner's jobs (background processes and watches) in one panel.
    fn jobs_view(&self, attached: Option<&str>) -> Option<JobsView> {
        let mut merged: Option<JobsView> = None;
        for view in self.owners().filter_map(|tool| tool.jobs_view(attached)) {
            match &mut merged {
                None => merged = Some(view),
                Some(merged) => {
                    merged.jobs.extend(view.jobs);
                    if merged.screen.is_empty() {
                        merged.screen = view.screen;
                        merged.cursor = view.cursor;
                    }
                }
            }
        }
        merged
    }
    fn kill_job(&self, name: &str) -> Result<String, String> {
        first_ok(self.owners().map(|tool| tool.kill_job(name)))
    }
    fn dismiss_job(&self, name: &str) -> Result<String, String> {
        first_ok(self.owners().map(|tool| tool.dismiss_job(name)))
    }
    fn accepts_adoption(&self) -> bool {
        self.owners().any(|tool| tool.accepts_adoption())
    }
    fn adopt_job(&self, job: crate::tools::exec::background::AdoptedJob) -> Result<String, String> {
        match self.owners().find(|tool| tool.accepts_adoption()) {
            Some(tool) => tool.adopt_job(job),
            None => Err(format!("{} does not accept foreground jobs", self.name)),
        }
    }
}

/// How a member is addressed inside its group.
#[derive(Clone, Copy, Debug)]
pub enum Spec {
    /// `field = value` selects the member.
    Select(&'static str, &'static str),
    /// Each `(merged, recorded)` action of the successor runs the member's `recorded` action.
    Actions(&'static [(&'static str, &'static str)]),
}

/// One consolidated tool: its members keep their owners, schemas and approval rules.
#[derive(Debug)]
pub struct Group {
    pub successor: &'static str,
    /// Leading description; `None` keeps the native successor's own description.
    pub intro: Option<&'static str>,
    pub selector_note: &'static str,
    /// The facade exists only while the successor itself is registered.
    pub native_required: bool,
    pub members: &'static [(&'static str, Spec)],
}

/// Registry tools folded into one model-facing tool. Members stay separately registered by their
/// owners and integrations configuration; the registry exposes only the successor
/// (`ext::Registry::register`). Recorded calls of a member project onto the successor
/// (`contracts::project_history`).
pub const GROUPS: &[Group] = &[
    Group {
        successor: "dap",
        intro: None,
        selector_note: "Installed debugger adapter; omit for automatic selection",
        native_required: false,
        members: &[],
    },
    Group {
        successor: "inspect",
        intro: Some(
            "Bounded offline readers for local files and retained citations; pass `kind` plus that reader's own arguments (an invalid call returns the reader's full contract). Each reader keeps its own limits and approval rules; results are untrusted observations, never verification.",
        ),
        selector_note: "Which reader handles the call",
        native_required: false,
        members: &[
            ("document", Spec::Select("kind", "document")),
            ("data", Spec::Select("kind", "data")),
            ("database", Spec::Select("kind", "database")),
            ("visual", Spec::Select("kind", "visual")),
            ("audio", Spec::Select("kind", "audio")),
            ("video", Spec::Select("kind", "video")),
            ("runtime_inspect", Spec::Select("kind", "trace")),
            ("network_inspect", Spec::Select("kind", "har")),
        ],
    },
    Group {
        successor: "history",
        intro: Some(
            "Read-only Dial history for this project; pass `source` plus that source's own arguments.",
        ),
        selector_note: "Which history source",
        native_required: false,
        members: &[
            ("archive", Spec::Select("source", "sessions")),
            ("code_history", Spec::Select("source", "changes")),
        ],
    },
    Group {
        successor: "bg",
        intro: None,
        selector_note: "",
        native_required: false,
        members: &[(
            "monitor",
            Spec::Actions(&[
                ("watch", "start"),
                ("watches", "list"),
                ("unwatch", "stop"),
                ("saved", "saved"),
                ("restore", "restore"),
                ("forget", "forget"),
            ]),
        )],
    },
    Group {
        successor: "evidence",
        intro: None,
        selector_note: "",
        native_required: true,
        members: &[
            (
                "research",
                Spec::Actions(&[
                    ("research_create", "create"),
                    ("research_update", "update"),
                    ("research_resume", "resume"),
                ]),
            ),
            (
                "autoresearch",
                Spec::Actions(&[
                    ("experiment_init", "init"),
                    ("experiment_run", "run"),
                    ("experiment_log", "log"),
                    ("experiment_notes", "notes"),
                    ("experiment_status", "status"),
                ]),
            ),
        ],
    },
    Group {
        successor: "memory",
        intro: None,
        selector_note: "codebase: saved project understanding (architecture, impact, navigate, refresh, remember, forget, invalidate, recall); omit for facts",
        native_required: true,
        members: &[("codebase_memory", Spec::Select("store", "codebase"))],
    },
    Group {
        successor: "git",
        intro: None,
        selector_note: "Forge operations through the configured CLI; omit for local Git/jj actions",
        native_required: true,
        members: &[
            ("github", Spec::Select("forge", "github")),
            ("glab_mr", Spec::Select("forge", "gitlab")),
            ("gitea_pr", Spec::Select("forge", "gitea")),
        ],
    },
];

/// The group a successor or member name belongs to.
pub fn group_of(name: &str) -> Option<&'static Group> {
    GROUPS.iter().find(|group| {
        group.successor == name
            || (group.successor == "dap" && name.starts_with("dap_"))
            || group.members.iter().any(|(m, _)| *m == name)
    })
}

/// The successor that now exposes a member tool.
pub fn successor_of(member: &str) -> Option<&'static str> {
    match member {
        "tool_load" => return Some("tool_search"),
        "skill_manage" => return Some("skill"),
        "rewind" => return Some("checkpoint"),
        "side_request" => return Some("task"),
        "request_directory" => return Some("ask"),
        "ast_search" => return Some("grep"),
        _ => {}
    }
    if member == "web_fetch" {
        return Some("read");
    }
    if member
        .strip_prefix("dap_")
        .is_some_and(|adapter| !adapter.is_empty())
    {
        return Some("dap");
    }
    GROUPS
        .iter()
        .find(|group| group.members.iter().any(|(m, _)| *m == member))
        .map(|group| group.successor)
}

/// A member's recorded call as the successor's arguments; `None` for a shape it never accepted.
pub fn successor_call(member: &str, args: &Value) -> Option<(&'static str, Value)> {
    match member {
        "tool_load" | "skill_manage" | "rewind" | "side_request" | "request_directory"
        | "ast_search" => {
            let mut call = args.clone();
            let object = call.as_object_mut()?;
            match member {
                "tool_load" => {
                    if object
                        .keys()
                        .any(|key| !matches!(key.as_str(), "names" | "unload"))
                    {
                        return None;
                    }
                }
                "skill_manage" => {
                    if !matches!(
                        object.get("action").and_then(Value::as_str),
                        Some("create" | "update" | "delete")
                    ) {
                        return None;
                    }
                }
                "side_request" => {
                    let action = match object
                        .get("action")
                        .and_then(Value::as_str)
                        .unwrap_or("start")
                    {
                        "start" => "question",
                        "status" => "question_status",
                        "wait" => "question_wait",
                        "cancel" => "question_cancel",
                        _ => return None,
                    };
                    object.insert("action".into(), json!(action));
                }
                "rewind" | "request_directory" => {
                    if object.contains_key("action") {
                        return None;
                    }
                    object.insert(
                        "action".into(),
                        json!(if member == "rewind" {
                            "rewind"
                        } else {
                            "directory"
                        }),
                    );
                }
                "ast_search" => {
                    if object
                        .keys()
                        .any(|key| !matches!(key.as_str(), "pattern" | "lang" | "path" | "limit"))
                    {
                        return None;
                    }
                    object.insert("ast".into(), json!(true));
                }
                _ => return None,
            }
            return Some((successor_of(member)?, call));
        }
        _ => {}
    }
    if member == "web_fetch" {
        let mut call = args.clone();
        let object = call.as_object_mut()?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "url" | "raw" | "max_chars"))
        {
            return None;
        }
        let url = object.remove("url")?;
        if !url.as_str().is_some_and(crate::tools::net::web::is_url) {
            return None;
        }
        object.insert("path".into(), url);
        object.insert("fetch".into(), json!(true));
        return Some(("read", call));
    }
    if let Some(adapter) = member
        .strip_prefix("dap_")
        .filter(|adapter| !adapter.is_empty())
    {
        let mut call = args.clone();
        let object = call.as_object_mut()?;
        if object.contains_key("adapter") {
            return None;
        }
        object.insert("adapter".into(), json!(adapter));
        return Some(("dap", call));
    }
    let group = GROUPS
        .iter()
        .find(|group| group.members.iter().any(|(m, _)| *m == member))?;
    let (_, spec) = group.members.iter().find(|(m, _)| *m == member)?;
    let mut call = args.clone();
    let object = call.as_object_mut()?;
    match spec {
        Spec::Select(field, value) => {
            if object.contains_key(*field) {
                return None;
            }
            object.insert((*field).into(), json!(value));
        }
        Spec::Actions(actions) => {
            let recorded = object.get("action")?.as_str()?;
            let (merged, _) = actions.iter().find(|(_, r)| *r == recorded)?;
            object.insert("action".into(), json!(merged));
        }
    }
    Some((group.successor, call))
}

/// The member a successor call reaches and the member's own arguments, from the group table alone
/// (no registry). Policy that predates consolidation (one-shot watch approval, image capture,
/// guidance) keys on this member view; `None` for a successor's own calls.
pub fn member_call(name: &str, args: &Value) -> Option<(&'static str, Value)> {
    let action = args.get("action").and_then(Value::as_str);
    let routed = match (name, action) {
        ("skill", Some("load")) => Some(("skill", None)),
        ("skill", Some("create" | "update" | "delete")) => Some(("skill_manage", action)),
        ("checkpoint", Some("start")) => Some(("checkpoint", None)),
        ("checkpoint", Some("rewind")) => Some(("rewind", None)),
        ("task", Some("question")) => Some(("side_request", Some("start"))),
        ("task", Some("question_status")) => Some(("side_request", Some("status"))),
        ("task", Some("question_wait")) => Some(("side_request", Some("wait"))),
        ("task", Some("question_cancel")) => Some(("side_request", Some("cancel"))),
        ("ask", Some("directory")) => Some(("request_directory", None)),
        ("ask", Some("questions")) => Some(("ask", None)),
        _ => None,
    };
    if let Some((owner, original_action)) = routed {
        let mut own = args.clone();
        let object = own.as_object_mut()?;
        match original_action {
            Some(action) => {
                object.insert("action".into(), json!(action));
            }
            None => {
                object.remove("action");
            }
        }
        return Some((owner, own));
    }
    if name == "tool_search" && (args.get("names").is_some() || args.get("unload").is_some()) {
        let mut own = args.clone();
        let object = own.as_object_mut()?;
        object.remove("query");
        object.remove("load");
        return Some(("tool_load", own));
    }
    if name == "grep" && args.get("ast").and_then(Value::as_bool) == Some(true) {
        let mut own = args.clone();
        let object = own.as_object_mut()?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "pattern" | "lang" | "path" | "limit" | "ast"))
        {
            return None;
        }
        object.remove("ast");
        return Some(("ast_search", own));
    }
    if name == "read" && args.get("fetch").and_then(Value::as_bool) == Some(true) {
        let mut own = args.clone();
        let object = own.as_object_mut()?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "path" | "fetch" | "raw" | "max_chars"))
        {
            return None;
        }
        let path = object.remove("path")?;
        if !path.as_str().is_some_and(crate::tools::net::web::is_url) {
            return None;
        }
        object.remove("fetch");
        object.insert("url".into(), path);
        return Some(("web_fetch", own));
    }
    let group = GROUPS.iter().find(|group| group.successor == name)?;
    for (member, spec) in group.members {
        match spec {
            Spec::Select(field, value) => {
                if args.get(*field).and_then(Value::as_str) == Some(*value) {
                    let mut own = args.clone();
                    own.as_object_mut()?.remove(*field);
                    return Some((member, own));
                }
            }
            Spec::Actions(actions) => {
                let action = args.get("action").and_then(Value::as_str);
                if let Some((_, recorded)) =
                    actions.iter().find(|(merged, _)| Some(*merged) == action)
                {
                    let mut own = args.clone();
                    own["action"] = json!(recorded);
                    return Some((member, own));
                }
            }
        }
    }
    None
}

/// Minimal routed arguments for a selector-only successor (no native tool), so policy probes such
/// as a child's read-only check reach a real member instead of the conservative unrouted answer.
pub fn probe(successor: &str) -> Option<Value> {
    let group = GROUPS
        .iter()
        .find(|group| group.successor == successor && !group.native_required)?;
    match group.members.first()?.1 {
        Spec::Select(field, value) => Some(json!({ field: value })),
        Spec::Actions(_) => None,
    }
}

/// `(name, args)` as the owning tool sees them: a member's own view for a routed successor call,
/// otherwise unchanged.
pub fn owner_view(name: &str, args: &Value) -> (String, Value) {
    if name == "dap"
        && let Some(adapter) = args.get("adapter").and_then(Value::as_str)
    {
        let mut own = args.clone();
        if let Some(object) = own.as_object_mut() {
            object.remove("adapter");
        }
        return (format!("dap_{adapter}"), own);
    }
    match member_call(name, args) {
        Some((member, own)) => (member.to_owned(), own),
        None => (name.to_owned(), args.clone()),
    }
}

/// The first sentence of a description, for one compact member line.
fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    match text.find(". ") {
        Some(end) if end < 320 => &text[..=end],
        _ => text.get(..320).map_or(text, |head| {
            head.rfind(' ').map_or(head, |cut| &head[..cut])
        }),
    }
}

/// Build a group's facade. `None` unless at least one member is present and, when required, the
/// native successor too.
pub fn build(
    group: &Group,
    native: Option<Arc<dyn Tool>>,
    present: &[(&'static str, Arc<dyn Tool>)],
) -> Option<Facade> {
    if present.is_empty() || (group.native_required && native.is_none()) {
        return None;
    }
    let mut members = Vec::new();
    let mut lines = Vec::new();
    for (name, spec) in group.members {
        let Some((_, tool)) = present.iter().find(|(n, _)| n == name) else {
            continue;
        };
        let summary = first_sentence(
            tool.schema()["function"]["description"]
                .as_str()
                .unwrap_or_default(),
        )
        .to_owned();
        match spec {
            Spec::Select(field, value) => {
                members.push(Member::select(field, *value, Arc::clone(tool)));
                lines.push(format!("{field}={value}: {summary}"));
            }
            Spec::Actions(actions) => {
                for (merged, recorded) in *actions {
                    members.push(Member::action(merged, Some(recorded), Arc::clone(tool)));
                }
                let names: Vec<&str> = actions.iter().map(|(merged, _)| *merged).collect();
                lines.push(format!("{} (former {name}): {summary}", names.join("/")));
            }
        }
    }
    let intro = match (group.intro, &native) {
        (Some(intro), _) => intro.to_owned(),
        (None, Some(native)) => native.schema()["function"]["description"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        (None, None) => String::new(),
    };
    let description = format!("{intro}\n{}", lines.join("\n")).trim().to_owned();
    Some(Facade::new(
        group.successor,
        description,
        group.selector_note,
        native,
        members,
    ))
}

impl Facade {
    /// Routes that would shadow each other: a merged action equal to a native action, a selector
    /// field equal to a native property, or one selector value used twice. Empty when sound.
    pub fn collisions(&self) -> Vec<String> {
        let native = self.native.as_ref().map(|tool| tool.schema());
        let native_properties = native
            .as_ref()
            .and_then(|schema| schema.pointer("/function/parameters/properties"))
            .and_then(Value::as_object);
        let native_actions: Vec<&str> = native_properties
            .and_then(|properties| properties.get("action"))
            .and_then(|action| action.get("enum"))
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut seen: Vec<String> = Vec::new();
        let mut found = Vec::new();
        for member in &self.members {
            let route = match &member.route {
                Route::Select { field, value } => {
                    if native_properties.is_some_and(|properties| properties.contains_key(*field)) {
                        found.push(format!("selector {field} is a native property"));
                    }
                    if member
                        .tool
                        .schema()
                        .pointer(&format!("/function/parameters/properties/{field}"))
                        .is_some()
                    {
                        found.push(format!(
                            "selector {field} is a property of {}",
                            member.tool.name()
                        ));
                    }
                    format!("{field}={value}")
                }
                Route::Action { merged, .. } => {
                    if native_actions.contains(merged) {
                        found.push(format!("action {merged} is a native action"));
                    }
                    format!("action={merged}")
                }
            };
            if seen.contains(&route) {
                found.push(format!("{route} routes twice"));
            }
            seen.push(route);
        }
        found
    }
}

/// The first success, or the last refusal.
fn first_ok<T>(results: impl Iterator<Item = Result<T, String>>) -> Result<T, String> {
    let mut last = Err("no owner".to_owned());
    for result in results {
        if result.is_ok() {
            return result;
        }
        last = result;
    }
    last
}

#[cfg(test)]
mod tests;
