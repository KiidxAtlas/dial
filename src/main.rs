mod args;
mod chat;
mod cmd;
mod dispatch;
mod overlay;

use anyhow::Result;
use args::Cli;
use clap::Parser;
use dial::repl::{OutputFormat, PrintExit, PrintUi};
use dispatch::real_main;

/// Maps a CLI subcommand's exit code onto the process: 0 is success, anything else exits with it.
fn exit_with(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        flush_or_exit(code)
    }
}

fn main() {
    dial::install_tls_provider();
    // Re-exec target for isolated document parsing; never reaches the CLI parser.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == dial::tools::documents::DOCUMENT_WORKER_ARG)
    {
        dial::tools::documents::document_worker_main();
    }
    // Re-exec target for the built-in shell (`shell.backend = "embedded"`); never reaches the CLI parser.
    #[cfg(feature = "embedded-shell")]
    {
        let mut args = std::env::args_os().skip(1);
        if args
            .next()
            .is_some_and(|a| a == dial::embedded_shell::HOST_ARG)
        {
            let script = args
                .next()
                .map(std::path::PathBuf::from)
                .unwrap_or_default();
            let state = args.next().map(std::path::PathBuf::from);
            std::process::exit(dial::embedded_shell::run_host_with_state(
                &script,
                state.as_deref(),
            ));
        }
    }
    let cli = Cli::parse();
    if cli.build_info {
        println!(
            "{}",
            serde_json::to_string_pretty(&dial::build_info::report()).unwrap()
        );
        flush_or_exit(0);
    }
    let format = dial::cli::cli_modes::resolve(cli.mode.as_deref(), cli.no_ui)
        .ok()
        .flatten()
        .map_or(cli.output_format, |mode| mode.output);
    let started = std::time::Instant::now();
    if let Err(error) = real_main(cli) {
        if let Some(exit) = error.downcast_ref::<PrintExit>() {
            flush_or_exit(exit.0);
        }
        eprintln!("error: {error:#}");
        if format != OutputFormat::Text {
            let mut ui = PrintUi::new(format, Vec::new());
            let _ = ui.finish(None, Some(&format!("{error:#}")), None, started.elapsed());
        }
        flush_or_exit(1);
    }
    flush_or_exit(0);
}

/// Drains buffered stdout (bounded) so final output is never lost, then exits with `code`.
fn flush_or_exit(code: i32) -> ! {
    dial::telemetry::shutdown();
    if let Err(error) = dial::flush_stdout_on_exit() {
        eprintln!("error: stdout flush failed: {error}");
        std::process::exit(if code == 0 { 1 } else { code });
    }
    std::process::exit(code)
}
