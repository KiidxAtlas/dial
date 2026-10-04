//! Operator controls for automatic self-improvement; normal use requires none of these commands.
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use dial::{
    agent::Runtime,
    config::types::{Config, Outcome, Proposal},
    learn::{auto, harvest, improve, probe},
    storage::{paths::Layout, store::Store},
};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{fs, io::Write, path::PathBuf, thread, time::Duration};

#[derive(Subcommand)]
pub enum Cmd {
    /// Resolve model capabilities and pin the local checkpoint.
    Doctor,
    /// Explicit external verdict; an unscored answer is not a success or a failure.
    Outcome { trajectory: i64, #[arg(long)] score: f64, #[arg(long)] source: String, #[arg(long)] reference: Option<String> },
    Diagnose,
    Propose { file: PathBuf },
    Train,
    Harvest,
    /// Run one full cycle now, regardless of the schedule threshold.
    Cycle,
    Watch { #[arg(long, default_value_t = 10)] poll_secs: u64 },
    Status,
    Versions,
    Rollback,
    /// Enable automatic background cycles for this project.
    Enable,
    /// Disable pending and future automatic cycles (does not interrupt a running cycle).
    Disable,
    #[command(hide = true)]
    Background { request: PathBuf },
}

pub fn run(cmd: Cmd, layout: &Layout, mut config: Config) -> Result<()> {
    if let Cmd::Background { request } = cmd { return auto::background(&request); }
    auto::prepare_state(layout)?;
    config.require_model()?;
    let store = Store::open(&config.state_dir, &config.model, &config.use_case, &config.harness)?;
    let mut progress = |s: &str| eprintln!("dial improve: {s}");
    match cmd {
        Cmd::Background { .. } => unreachable!(),
        Cmd::Enable | Cmd::Disable => {
            let enabled = matches!(cmd, Cmd::Enable);
            store.set_meta("auto_enabled", if enabled { "true" } else { "false" })?;
            print_json(&json!({"enabled":enabled,"configured":config.improve.enabled}))?;
            if enabled { auto::maybe_spawn(layout, &config, &store)?; }
        }
        Cmd::Status => print_json(&json!({
            "active":store.active()?, "automatic":auto::enabled(&config, &store)?, "due":auto::due(&config, &store)?,
            "scheduler":meta_json(&store,"auto_status")?,"last_cycle":meta_json(&store,"last_cycle_report")?,
            "metrics":store.metrics()?,"capabilities":meta_json(&store,"serving_capabilities")?,
            "layer_sensitivity":meta_json(&store,"layer_sensitivity")?,
            "step_probe":probe::load(&store)?.map(|p|json!({"kind":"event-feature logistic regression","auc":p.auc,"trained_on":p.trained_on})),
            "last_rollback":store.get_meta("last_rollback")?,"state_dir":config.state_dir,
            "unavailable":["expert_growth","routing_calibration","attention_retrofit","hidden_state_neural_probe"]
        }))?,
        Cmd::Versions => print_json(&store.versions()?)?,
        Cmd::Outcome { trajectory, score, source, reference } => {
            store.outcome(&Outcome { trajectory, score, source, reference })?;
            print_json(&json!({"trajectory":trajectory,"recorded":true}))?;
            auto::maybe_spawn(layout, &config, &store)?;
        }
        Cmd::Rollback => {
            let control = auto::lock_file(&config.state_dir.join("control.lock"))?;
            control.lock_exclusive()?;
            let parent = store.active()?.parent.context("initial version has no predecessor")?;
            improve::verify_adapter(&store, &store.version(parent)?)?;
            print_json(&store.rollback()?)?;
        }
        Cmd::Harvest => {
            let control = auto::lock_file(&config.state_dir.join("control.lock"))?;
            control.lock_exclusive()?;
            let evaluation = improve::build_evaluation(&config, &store)?;
            print_json(&harvest::harvest(&config, &store, &evaluation.held_out)?)?;
        }
        Cmd::Watch { poll_secs } => {
            ensure!((1..=3600).contains(&poll_secs), "poll_secs must be 1..3600");
            loop {
                if auto::due(&config, &store)? {
                    let control = auto::lock_file(&config.state_dir.join("control.lock"))?;
                    if control.try_lock_exclusive().is_ok() {
                        let _compute = if config.uses_local_compute() { Some(auto::compute_lock(layout)?) } else { None };
                        match auto::execute(&mut config.clone(), &store, false, &mut progress) {
                            Ok(report) => print_json(&report)?,
                            Err(error) => print_json(&json!({"cycle_error":format!("{error:#}")}))?,
                        }
                    }
                }
                thread::sleep(Duration::from_secs(poll_secs));
            }
        }
        command => {
            let control = auto::lock_file(&config.state_dir.join("control.lock"))?;
            control.lock_exclusive()?;
            let _compute = if config.uses_local_compute() { Some(auto::compute_lock(layout)?) } else { None };
            if matches!(command, Cmd::Cycle) {
                return print_json(&auto::execute(&mut config, &store, true, &mut progress)?);
            }
            let capabilities = improve::pin_models(&mut config, &store, matches!(command, Cmd::Doctor | Cmd::Diagnose))?;
            match command {
                Cmd::Doctor => print_json(&json!({"serving":capabilities,"feedback":config.feedback_model()}))?,
                Cmd::Diagnose => {
                    let p = probe::load(&store)?;
                    let signals = improve::collect_signals(&store, &store.active()?, p.as_ref())?;
                    let (_, report) = improve::diagnose(&mut Runtime::new(), &config, &store, &signals)?;
                    print_json(&report)?;
                }
                Cmd::Train | Cmd::Propose { .. } => {
                    let proposal = match command {
                        Cmd::Propose { file } => {
                            ensure!(fs::metadata(&file)?.len() <= 65536, "proposal exceeds 64 KiB");
                            serde_json::from_slice::<Proposal>(&fs::read(file)?)?
                        }
                        _ => Proposal { diagnosis:"Operator-requested training from verified experience".into(), operation:"lora".into(), harness:None },
                    };
                    print_json(&improve::apply(&mut Runtime::new(), &config, &store, &proposal, &mut progress)?)?;
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}

fn meta_json(store: &Store, key: &str) -> Result<Option<Value>> {
    store.get_meta(key)?.map(|s| serde_json::from_str(&s).context("invalid stored report")).transpose()
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
