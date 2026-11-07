use super::*;
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Condvar, Mutex},
    time::Duration,
};

#[test]
fn child_preparation_overlaps_but_every_computed_effect_keeps_live_authority() {
    struct Bounded(Arc<(Mutex<BTreeSet<String>>, Condvar)>);
    impl Tool for Bounded {
        fn name(&self) -> &str {
            "fixture_bounded"
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":self.name(),"parameters":{
                "type":"object","properties":{"path":{"type":"string"},"effect":{"type":"string"}},"required":["path"]}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            true
        }
        fn manages_workspace_serialization(&self) -> bool {
            true
        }
        fn resources(
            &self,
            _: &Workspace,
            args: &Value,
        ) -> Option<tools::surface::resources::Access> {
            Some(tools::surface::resources::Access {
                reads: vec![],
                writes: vec![args["path"].as_str()?.into()],
            })
        }
        fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
            let path = args["path"].as_str().unwrap();
            let (seen, changed) = &*self.0;
            let mut seen = seen.lock().unwrap();
            seen.insert(path.into());
            changed.notify_all();
            let (seen, _) = changed
                .wait_timeout_while(seen, Duration::from_secs(3), |seen| seen.len() < 2)
                .unwrap();
            if seen.len() < 2 {
                return ToolOutput::err("native child preparation did not overlap");
            }
            drop(seen);
            let effect = ctx
                .workspace
                .resolve(args["effect"].as_str().unwrap_or(path))
                .unwrap();
            if let Err(reason) =
                ctx.authorize_mutation(self.name(), std::slice::from_ref(&effect), false)
            {
                return ToolOutput::err(reason);
            }
            let _commit = ctx.workspace.serial_operation();
            std::fs::write(&effect, "committed").unwrap();
            ToolOutput::ok("committed".into()).with_changes([effect])
        }
    }
    #[derive(Default)]
    struct Owner {
        computed: Vec<PathBuf>,
        finished: usize,
    }
    impl Approver for Owner {
        fn authorize(
            &mut self,
            _: &llm::ToolCall,
            _: &str,
            _: bool,
        ) -> Result<(), approval::Denial> {
            Ok(())
        }
        fn authorize_mutation(
            &mut self,
            _: &llm::ToolCall,
            _: &str,
            paths: &[PathBuf],
            _: bool,
            _: &Arc<AtomicBool>,
            _: subagents::MutationScope,
        ) -> Result<(), String> {
            self.computed.extend_from_slice(paths);
            Ok(())
        }
        fn tool_finished(&mut self, _: &str) {
            self.finished += 1;
        }
    }
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let harness = Harness {
        tools: vec!["fixture_bounded".into()],
        ..Harness::default()
    };
    let workspace = Workspace::new(root.path()).unwrap();
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut registry = Registry::new();
    let observed = Arc::new((Mutex::new(BTreeSet::new()), Condvar::new()));
    registry.register(Arc::new(Bounded(observed.clone())));
    let env = Env {
        config: &config,
        workspace: &workspace,
        ext: &registry,
        layout: None,
        harness: &harness,
        interrupt: &interrupt,
        usage: None,
        context_cap: None,
        memory: None,
        agents_messages: &[],
        plan_mode: false,
    };
    let parsed = subagents::parse(
        &json!({"agent":"worker","task":"Bounded fixtures","solution_space":"fixture files"}),
        &config,
    )
    .unwrap();
    let job = &parsed.jobs[0];
    let names = toolset(job.spec, &harness.tools, &registry);
    let schemas = schemas(env, &names).unwrap();
    let calls = [
        llm::ToolCall {
            id: "a".into(),
            name: "fixture_bounded".into(),
            arguments: json!({"path":"a.txt"}),
            malformed_arguments: false,
        },
        llm::ToolCall {
            id: "b".into(),
            name: "fixture_bounded".into(),
            arguments: json!({"path":"b.txt","effect":"escaped.txt"}),
            malformed_arguments: false,
        },
    ];
    let children = ChildBackends::new(env, None).unwrap();
    let mut owner = Owner::default();
    let results = call::run_group(
        env,
        job,
        ToolSurface {
            schemas: &schemas,
            names: &names,
            provenance: approval::CallProvenance::Structured,
        },
        &calls,
        &mut owner,
        call::GroupContext {
            children: &children,
            callback_gate: None,
            pending: &mut vec![],
            steps: 1,
        },
    );
    assert!(!results[0].0.is_error, "{}", results[0].0.text);
    assert!(
        results[1]
            .0
            .text
            .contains("exceeded its admitted resource contract"),
        "{}",
        results[1].0.text
    );
    assert_eq!(observed.0.lock().unwrap().len(), 2);
    assert_eq!(owner.computed, [workspace.root().join("a.txt")]);
    assert_eq!(owner.finished, 2, "one completion per tool call");
    assert!(root.path().join("a.txt").is_file());
    assert!(!root.path().join("b.txt").exists());
    assert!(!root.path().join("escaped.txt").exists());
}
