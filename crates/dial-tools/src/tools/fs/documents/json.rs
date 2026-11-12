//! `file.json?q=<jq filter>` queries: a jq program (jaq 3.x)
//! run over a JSON document, or over each line of a JSONL/NDJSON document, returning a page of
//! results. Evaluation runs on a worker thread under a deadline; the worker checks a cancel flag
//! before every output, so an unbounded generator such as `repeat(1)` stops at the deadline on
//! every platform. On unix the whole read also runs in the documents fork isolation (see `mod.rs`).
use super::{MAX_CONTENT, MAX_INPUT, bounded};
use crate::tools::fs::select::MAX_READ_OUTPUT;
use jaq_core::{Ctx, DataT, Exn, Lut, Vars, compile::Compiler, data, load};
use jaq_json::Val;
use jaq_std::input::{self, HasInputs, Inputs, RcIter};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

const MAX_VISITS: usize = 1_000_000;
const MAX_QUERY: usize = 4096;
/// Wall-clock budget for one query, from parsing the query through the last result.
const QUERY_DEADLINE: Duration = Duration::from_secs(60);

type Filter = jaq_core::Filter<Kind>;

/// Data kind of the query filters: jaq-json values, with natives reaching the invocation
/// through [`Data`]. The module loader is sandboxed (no search paths), and `inputs`/`input`
/// see an empty stream, so a query can only read the document it is given.
struct Kind;

impl DataT for Kind {
    type Data<'a> = &'a Data<'a>;
    type V<'a> = Val;
}

struct Data<'a> {
    lut: &'a Lut<Kind>,
    inputs: Inputs<'a, Val>,
}

impl<'a> data::HasLut<'a, Kind> for &'a Data<'a> {
    fn lut(&self) -> &'a Lut<Kind> {
        self.lut
    }
}

impl<'a> HasInputs<'a, Val> for &'a Data<'a> {
    fn inputs(&self) -> Inputs<'a, Val> {
        self.inputs
    }
}

struct Options {
    query: String,
    offset: usize,
    limit: usize,
    raw: bool,
}

/// Paths `?q=` queries: JSON, JSONL/NDJSON, YAML and TOML, optionally gzip/zstd/xz/bzip2
/// compressed (`data.jsonl.gz`).
pub(super) fn is_queryable(path: &str) -> bool {
    source_format(path).is_some()
}

/// The source format of a queryable path, by its extension under any compression suffix.
fn source_format(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    let base = super::without_compression(&lower);
    [
        (".json", "JSON"),
        (".jsonl", "JSONL"),
        (".ndjson", "NDJSON"),
        (".yaml", "YAML"),
        (".yml", "YAML"),
        (".toml", "TOML"),
    ]
    .iter()
    .find_map(|(suffix, format)| base.ends_with(suffix).then_some(*format))
}
fn decode(input: &str) -> Result<String, String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .ok_or("incomplete query percent escape")?;
            let hex = std::str::from_utf8(hex).map_err(|_| "invalid query percent escape")?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| "invalid query percent escape")?);
            i += 3;
        } else {
            // '+' is literal, not HTML-form whitespace; jq's `+` must survive.
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "query option is not UTF-8".into())
}
fn options(input: &str) -> Result<Options, String> {
    let (mut query, mut offset, mut limit, mut raw) = (".".to_string(), 0usize, 100usize, false);
    let mut seen = std::collections::HashSet::new();
    for pair in input.split('&') {
        let (name, value) = pair
            .split_once('=')
            .ok_or("JSON query options require name=value")?;
        if !seen.insert(name) {
            return Err(format!("duplicate JSON option {name:?}"));
        }
        let value = decode(value)?;
        match name {
            "q" => query = value,
            "offset" => {
                offset = value
                    .parse()
                    .map_err(|_| "JSON offset must be 0..1000000")?
            }
            "limit" => limit = value.parse().map_err(|_| "JSON limit must be 1..1000")?,
            "raw" => {
                raw = match value.as_str() {
                    "true" | "1" => true,
                    "false" | "0" => false,
                    _ => return Err("JSON raw must be true or false".into()),
                }
            }
            "compact" if matches!(value.as_str(), "true" | "1") => {}
            "compact" => {
                return Err("JSON query results are always compact; compact must be true".into());
            }
            _ => {
                return Err(format!(
                    "unknown JSON option {name:?}; supported: q, offset, limit, raw, compact=true"
                ));
            }
        }
    }
    if offset > MAX_VISITS || !(1..=1000).contains(&limit) {
        return Err("JSON offset must be 0..1000000 and limit 1..1000".into());
    }
    if query.len() > MAX_QUERY {
        return Err(format!("jq query exceeds {MAX_QUERY} bytes"));
    }
    if query.trim().is_empty() {
        query = ".".into();
    }
    Ok(Options {
        query,
        offset,
        limit,
        raw,
    })
}
/// Compiles a jq program with the jaq standard library and jq definitions.
fn compile(query: &str) -> Result<Filter, String> {
    let arena = load::Arena::default();
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    // An empty search path: `import`/`include` cannot read files from disk.
    let loader = load::Loader::new(defs).with_std_read(&[]);
    let modules = loader
        .load(
            &arena,
            load::File {
                code: query,
                path: PathBuf::new(),
            },
        )
        .map_err(|errs| {
            let errors: Vec<_> = errs.into_iter().map(|(_, e)| e).collect();
            format!("invalid jq query: {errors:?}")
        })?;
    let run = jaq_core::native::run::<Kind>;
    let inputs = input::funs::<Kind>().into_vec().into_iter().map(run);
    let funs = jaq_core::funs::<Kind>()
        .chain(jaq_std::funs::<Kind>())
        .chain(jaq_json::funs::<Kind>())
        .chain(inputs);
    Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|errs| {
            let errors: Vec<_> = errs.into_iter().map(|(_, e)| e).collect();
            format!("invalid jq query: {errors:?}")
        })
}
struct Page<'a> {
    options: &'a Options,
    rows: String,
    total: usize,
    shown: usize,
    visits: usize,
    clipped: bool,
}
impl Page<'_> {
    fn push(&mut self, value: Val, source_line: Option<usize>) -> Result<(), String> {
        self.visits += 1;
        if self.visits > MAX_VISITS {
            return Err("jq query exceeds 1000000 results; narrow the query".into());
        }
        let index = self.total;
        self.total += 1;
        if index >= self.options.offset && self.shown < self.options.limit && !self.clipped {
            let text = render(&value, self.options.raw);
            let label = source_line.map_or_else(
                || format!("[result {index}]\n"),
                |n| format!("[result {index}; source line {n}]\n"),
            );
            if self.rows.len() + label.len() + text.len() + 1 > MAX_READ_OUTPUT - 4096 {
                if self.shown == 0 {
                    return Err("one JSON result exceeds the 16 KiB page budget; select a smaller field (results are never silently truncated)".into());
                }
                self.clipped = true;
            } else {
                self.rows.push_str(&label);
                self.rows.push_str(&text);
                self.rows.push('\n');
                self.shown += 1;
            }
        }
        Ok(())
    }
}
/// Compact JSON, or the bare string when `raw` is set and the result is a string (as `jq -r`).
fn render(value: &Val, raw: bool) -> String {
    match value {
        Val::TStr(text) if raw => jaq_json::bstr(&**text).to_string(),
        _ => value.to_string(),
    }
}
fn with_line(source_line: Option<usize>, message: String) -> String {
    match source_line {
        Some(n) => format!("source line {n}: {message}"),
        None => message,
    }
}
fn exn_message(exn: Exn<'_, Val>) -> String {
    match exn.get_err() {
        Ok(error) => error.to_string(),
        Err(exn) => match exn.get_halt() {
            Ok(code) => format!("halted with exit code {code}"),
            Err(_) => "unknown jq exception".into(),
        },
    }
}
/// Runs the program on one input and pages every result it yields. A runtime error stops the
/// query, as `jq` exits nonzero on it. The cancel flag is checked before every result.
fn feed(
    filter: &Filter,
    input: Val,
    source_line: Option<usize>,
    page: &mut Page<'_>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let no_inputs = RcIter::new(core::iter::empty::<Result<Val, String>>());
    let data = Data {
        lut: &filter.lut,
        inputs: &no_inputs,
    };
    let ctx = Ctx::<Kind>::new(&data, Vars::new(core::iter::empty::<Val>()));
    for output in filter.id.run((ctx, input)) {
        if cancel.load(Ordering::Relaxed) {
            return Err(with_line(source_line, "jq query cancelled".into()));
        }
        let value = output
            .map_err(|exn| with_line(source_line, format!("jq error: {}", exn_message(exn))))?;
        page.push(value, source_line)?;
    }
    Ok(())
}
pub(super) fn read(path: &Path, label: &str, input: &str) -> Result<String, String> {
    read_with_deadline(path, label, input, QUERY_DEADLINE)
}
/// [`read`] with an explicit deadline. Evaluation runs on a worker thread; when the deadline
/// passes first, the worker is told to stop and the caller gets a timeout error.
pub(super) fn read_with_deadline(
    path: &Path,
    label: &str,
    input: &str,
    deadline: Duration,
) -> Result<String, String> {
    let options = options(input)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    {
        let cancel = Arc::clone(&cancel);
        let path = path.to_path_buf();
        let label = label.to_string();
        std::thread::Builder::new()
            .name("jq-query".into())
            .spawn(move || {
                // The receiver is gone after a timeout; the send error is then expected.
                let _ = tx.send(evaluate(&path, &label, &options, &cancel));
            })
            .map_err(|e| format!("starting jq worker: {e}"))?;
    }
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            cancel.store(true, Ordering::Relaxed);
            Err(format!(
                "jq query timed out after {:.1}s; narrow the query or select fewer results",
                deadline.as_secs_f64()
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err("jq query worker failed".into()),
    }
}
fn evaluate(
    path: &Path,
    label: &str,
    options: &Options,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let filter = compile(&options.query)?;
    let meta = std::fs::metadata(path).map_err(|e| format!("reading JSON: {e}"))?;
    if !meta.is_file() || meta.len() > MAX_INPUT {
        return Err("JSON input must be a regular file no larger than 64 MiB".into());
    }
    let mut page = Page {
        options,
        rows: String::new(),
        total: 0,
        shown: 0,
        visits: 0,
        clipped: false,
    };
    let source_format = source_format(label).unwrap_or("JSON");
    let file = File::open(path).map_err(|e| format!("opening JSON: {e}"))?;
    let compressed = super::without_compression(&label.to_ascii_lowercase()).len() != label.len();
    if matches!(source_format, "JSONL" | "NDJSON") {
        let mut reader: Box<dyn BufRead> = if compressed {
            let bytes = bounded(file)?;
            Box::new(std::io::Cursor::new(
                super::archives::decompressed(&bytes)?.into_owned(),
            ))
        } else {
            Box::new(BufReader::new(file))
        };
        let mut line = Vec::new();
        let mut source_line = 0;
        let mut total_bytes = 0u64;
        loop {
            line.clear();
            let read = reader
                .by_ref()
                .take(MAX_CONTENT + 1)
                .read_until(b'\n', &mut line)
                .map_err(|e| format!("reading JSONL: {e}"))?;
            if read == 0 {
                break;
            }
            source_line += 1;
            total_bytes += read as u64;
            if line.len() as u64 > MAX_CONTENT || total_bytes > MAX_INPUT {
                return Err(format!(
                    "JSONL source line {source_line} exceeds content/input safety limits"
                ));
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value = jaq_json::read::parse_single(&line)
                .map_err(|e| format!("malformed JSONL/NDJSON at source line {source_line}: {e}"))?;
            feed(&filter, value, Some(source_line), &mut page, cancel)?;
        }
    } else {
        let raw = bounded(file)?;
        let bytes = super::archives::decompressed(&raw)?;
        for document in documents(source_format, &bytes)? {
            let value = jaq_json::read::parse_single(&document)
                .map_err(|e| format!("malformed {source_format}: {e}"))?;
            feed(&filter, value, None, &mut page, cancel)?;
        }
    }
    let next = options.offset.saturating_add(page.shown);
    let remaining = page.total.saturating_sub(next);
    let mut out = format!(
        "[{label}; source format {source_format}; jq query {:?}; result offset {}; {} of {} results; not editable source anchors]\n",
        options.query, options.offset, page.shown, page.total
    );
    out.push_str(&page.rows);
    if remaining > 0 {
        out.push_str(&format!(
            "[{remaining} more results; retain q/raw options and use &offset={next}&limit={}]\n",
            options.limit
        ));
    } else {
        out.push_str("[end of query results]\n");
    }
    if out.len() > MAX_READ_OUTPUT {
        return Err("JSON page including labels exceeds 20 KiB; reduce query/limit".into());
    }
    Ok(out)
}

/// The JSON text of each document in `bytes`: the document itself for JSON, the converted table
/// for TOML (datetimes become strings), and one value per `---` document for YAML (non-string
/// keys are stringified, tags dropped).
fn documents(format: &str, bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    match format {
        "JSON" => Ok(vec![bytes.to_vec()]),
        "TOML" => {
            let text = std::str::from_utf8(bytes).map_err(|_| "TOML is not UTF-8")?;
            let table: toml::Table =
                toml::from_str(text).map_err(|e| format!("malformed TOML: {e}"))?;
            Ok(vec![
                from_toml(toml::Value::Table(table))
                    .to_string()
                    .into_bytes(),
            ])
        }
        _ => {
            use serde::Deserialize as _;
            let text = std::str::from_utf8(bytes).map_err(|_| "YAML is not UTF-8")?;
            let mut out = Vec::new();
            for document in serde_norway::Deserializer::from_str(text) {
                let value = serde_norway::Value::deserialize(document)
                    .map_err(|e| format!("malformed YAML: {e}"))?;
                out.push(from_yaml(value).to_string().into_bytes());
            }
            Ok(out)
        }
    }
}

fn from_toml(value: toml::Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        toml::Value::String(s) => J::String(s),
        toml::Value::Integer(n) => J::from(n),
        toml::Value::Float(f) => serde_json::Number::from_f64(f).map_or(J::Null, J::Number),
        toml::Value::Boolean(b) => J::Bool(b),
        toml::Value::Datetime(d) => J::String(d.to_string()),
        toml::Value::Array(items) => J::Array(items.into_iter().map(from_toml).collect()),
        toml::Value::Table(table) => J::Object(
            table
                .into_iter()
                .map(|(key, value)| (key, from_toml(value)))
                .collect(),
        ),
    }
}

fn from_yaml(value: serde_norway::Value) -> serde_json::Value {
    use serde_json::Value as J;
    use serde_norway::Value as Y;
    match value {
        Y::Null => J::Null,
        Y::Bool(b) => J::Bool(b),
        Y::Number(n) => n
            .as_i64()
            .map(J::from)
            .or_else(|| n.as_u64().map(J::from))
            .or_else(|| {
                n.as_f64()
                    .and_then(serde_json::Number::from_f64)
                    .map(J::Number)
            })
            .unwrap_or(J::Null),
        Y::String(s) => J::String(s),
        Y::Sequence(items) => J::Array(items.into_iter().map(from_yaml).collect()),
        Y::Mapping(map) => J::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let key = match key {
                        Y::String(s) => s,
                        other => from_yaml(other).to_string(),
                    };
                    (key, from_yaml(value))
                })
                .collect(),
        ),
        Y::Tagged(tagged) => from_yaml(tagged.value),
    }
}
