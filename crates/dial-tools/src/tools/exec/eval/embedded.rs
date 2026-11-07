//! Embedded JavaScript engine for `eval` cells when no operator-installed Node 22+ resolves.
//!
//! The `dial` binary re-executes itself with [`WORKER_ARG`] under the same fork fence and fd-3
//! protocol as `driver.cjs`. Rust owns frame I/O, completion-value rewriting and the QuickJS
//! evaluator; `embedded/shim.js` owns cell semantics (tool callbacks, console capture, timers,
//! result frames). QuickJS is an ECMAScript engine, not Node: there are no Node APIs or modules.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::FromRawFd;
use std::rc::Rc;
use std::time::Duration;

use ast_grep_core::AstGrep;
use ast_grep_language::SupportLang;
use rquickjs::context::EvalOptions;
use rquickjs::convert::Coerced;
use rquickjs::{Context, Ctx, Exception, Function, Object, Runtime, Value};

/// Argument that makes `dial` run the embedded worker instead of the CLI.
pub const WORKER_ARG: &str = "--dial-eval-js-worker";

/// Same bound as the Node driver and the controller's frame reader.
const MAX_FRAME: usize = 12 * 1024 * 1024;
const SHIM: &str = include_str!("embedded/shim.js");

enum Line {
    Text(String),
    Overflow,
    Eof,
}

struct Frames {
    input: Box<dyn Read>,
    output: Box<dyn Write>,
    pending: Vec<u8>,
    discarding: bool,
}

impl Frames {
    /// Returns the next newline-delimited frame. An oversized frame is reported once and then
    /// discarded through its newline, mirroring the Node driver's overflow handling.
    fn read_line(&mut self) -> Result<Line, String> {
        loop {
            if let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
                let mut frame: Vec<u8> = self.pending.drain(..=newline).collect();
                frame.pop();
                if std::mem::take(&mut self.discarding) {
                    continue;
                }
                return Ok(Line::Text(String::from_utf8_lossy(&frame).into_owned()));
            }
            if self.discarding {
                self.pending.clear();
            } else if self.pending.len() > MAX_FRAME {
                self.pending.clear();
                self.discarding = true;
                return Ok(Line::Overflow);
            }
            let mut chunk = [0_u8; 16 * 1024];
            match self.input.read(&mut chunk) {
                Ok(0) if self.discarding || self.pending.is_empty() => return Ok(Line::Eof),
                Ok(0) => return Err("incomplete controller frame".into()),
                Ok(read) => self.pending.extend_from_slice(&chunk[..read]),
                Err(error) if is_retryable(&error) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    fn write_line(&mut self, text: &str) -> Result<(), String> {
        write_all_retrying(&mut self.output, text.as_bytes())?;
        write_all_retrying(&mut self.output, b"\n")
    }
}

fn is_retryable(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted)
}

fn write_all_retrying(output: &mut dyn Write, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        match output.write(bytes) {
            Ok(0) => return Err("controller closed the protocol pipe".into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if is_retryable(&error) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

type Shared = Rc<RefCell<Frames>>;

/// Installs the natives that `shim.js` captures and then removes from the global object.
fn install<'js>(ctx: &Ctx<'js>, frames: &Shared) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    let io_frames = Rc::clone(frames);
    globals.set(
        "__dial_read_line",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> rquickjs::Result<Object<'js>> {
                let line = io_frames
                    .borrow_mut()
                    .read_line()
                    .map_err(|error| Exception::throw_message(&ctx, &error))?;
                let object = Object::new(ctx.clone())?;
                match line {
                    Line::Text(text) => object.set("line", text)?,
                    Line::Overflow => object.set("overflow", true)?,
                    Line::Eof => object.set("eof", true)?,
                }
                Ok(object)
            },
        )?,
    )?;

    let write_frames = Rc::clone(frames);
    globals.set(
        "__dial_write_line",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, text: String| -> rquickjs::Result<()> {
                write_frames
                    .borrow_mut()
                    .write_line(&text)
                    .map_err(|error| Exception::throw_message(&ctx, &error))
            },
        )?,
    )?;

    globals.set(
        "__dial_completion_source",
        Function::new(ctx.clone(), |code: String| -> String {
            completion_source(&code)
        })?,
    )?;

    globals.set(
        "__dial_eval",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, source: String, promise: bool| -> rquickjs::Result<Value<'js>> {
                let mut options = EvalOptions::default();
                options.global = true;
                options.strict = false;
                options.promise = promise;
                ctx.eval_with_options::<Value<'js>, _>(source, options)
            },
        )?,
    )?;

    globals.set(
        "__dial_run_jobs",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>| {
                while ctx.execute_pending_job() {}
            },
        )?,
    )?;

    globals.set(
        "__dial_sleep",
        Function::new(ctx.clone(), |ms: f64| {
            if ms.is_finite() && ms > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(ms / 1000.0));
            }
        })?,
    )?;

    Ok(())
}

/// Rewrites a trailing top-level expression statement into `__dial_completion__ = (expr)`, so
/// async cells can report a completion value (QuickJS drops it under promise-mode evaluation).
/// Sources with parse errors are returned unchanged and fail with the engine's own SyntaxError.
fn completion_source(code: &str) -> String {
    let Ok(document) = AstGrep::try_new(code, SupportLang::JavaScript) else {
        return code.to_owned();
    };
    let root = document.root();
    if root.dfs().any(|node| node.is_error() || node.is_missing()) {
        return code.to_owned();
    }
    let Some(statement) = root.children().filter(|node| !node.is_extra()).last() else {
        return code.to_owned();
    };
    if statement.kind() != "expression_statement" {
        return code.to_owned();
    }
    let Some(expression) = statement
        .children()
        .find(|node| node.is_named() && !node.is_extra())
    else {
        return code.to_owned();
    };
    let range = expression.range();
    format!(
        "{}__dial_completion__ = ({}){}",
        &code[..range.start],
        &code[range.clone()],
        &code[range.end..]
    )
}

/// Renders a pending JavaScript exception as text for the worker's stderr.
fn describe(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    match error {
        rquickjs::Error::Exception => match ctx.catch().get::<Coerced<String>>() {
            Ok(Coerced(text)) => text,
            Err(_) => "embedded JavaScript engine raised an unprintable exception".into(),
        },
        other => other.to_string(),
    }
}

/// Runs the cell protocol on `input`/`output` until the input reaches EOF.
pub fn serve(input: Box<dyn Read>, output: Box<dyn Write>) -> Result<(), String> {
    let frames: Shared = Rc::new(RefCell::new(Frames {
        input,
        output,
        pending: Vec::new(),
        discarding: false,
    }));
    let runtime = Runtime::new().map_err(|error| error.to_string())?;
    let context = Context::full(&runtime).map_err(|error| error.to_string())?;
    context.with(|ctx| {
        install(&ctx, &frames).map_err(|error| describe(&ctx, error))?;
        ctx.eval::<(), _>(SHIM)
            .map_err(|error| describe(&ctx, error))
    })
}

/// Entry point for the re-executed worker process. Returns the process exit status.
pub fn worker_main() -> i32 {
    // SAFETY: the launcher maps the protocol pipe to fd 3 for this process only, so nothing else
    // owns that descriptor and this `File` becomes its sole owner.
    let output = unsafe { File::from_raw_fd(3) };
    match serve(Box::new(io::stdin()), Box::new(output)) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(io::stderr(), "{error}");
            1
        }
    }
}

#[cfg(test)]
mod tests;
