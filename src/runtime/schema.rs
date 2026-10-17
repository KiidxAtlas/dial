//! A JSON Schema validator for `task`'s `output_schema` and for checking tool arguments.
//!
//! Semantics follow draft 2020-12, plus the draft-07 array form of `items` with `additionalItems`.
//! Supported keywords:
//! - references: `$ref` to local JSON pointers (`#`, `#/$defs/x`, `#/definitions/x`, any `#/…`
//!   with `~0`/`~1` and percent-encoding), resolved against the root schema. Recursive schemas are
//!   allowed; a chain of `$ref` hops that never consumes a value is cut off after 64 hops.
//! - applicators: `allOf`, `anyOf`, `oneOf` (exactly one branch), `not`, `if`/`then`/`else`.
//! - any value: `type` (one name or a list), `enum`, `const`.
//! - strings: `minLength`, `maxLength` (characters), `pattern`, and `format` for `date-time`,
//!   `date`, `time`, `email`, `uri`, `uuid`, `ipv4`, `ipv6` and `hostname`. Other format names are
//!   annotations and are not enforced.
//! - numbers: `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum` (numeric form), `multipleOf`.
//! - objects: `properties`, `required`, `patternProperties`, `additionalProperties` (`false` or a
//!   schema; it applies only to keys matched by neither `properties` nor `patternProperties`),
//!   `propertyNames`, `minProperties`, `maxProperties`, `dependentRequired`.
//! - arrays: `items` (schema, or tuple form), `prefixItems`, `additionalItems`, `contains` with
//!   `minContains`/`maxContains`, `minItems`, `maxItems`, `uniqueItems`.
//! - annotations, accepted and ignored: `$schema`, `$id`, `$comment`, `title`, `description`,
//!   `default`, `examples`, `readOnly`, `writeOnly`, `deprecated`, `$defs`, `definitions`.
//!
//! `check_schema` rejects keywords that would change validity without being evaluated here
//! (`unevaluatedProperties`, `$dynamicRef`, …) and remote or unresolvable references, so callers
//! hear about them up front instead of receiving a validator that silently accepts too much.
//!
//! Unlike `calls::validate`, which checks flat tool arguments against dial's own schemas, this one
//! recurses, because a delegated result may be any shape the parent asked for.
use regex::Regex;
use serde_json::{Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;

const TYPES: &[&str] = &[
    "string", "number", "integer", "boolean", "object", "array", "null",
];
/// Keywords that change what is valid but are not evaluated here; refusing them beats pretending.
const UNSUPPORTED: &[&str] = &[
    "unevaluatedProperties",
    "unevaluatedItems",
    "dependentSchemas",
    "dependencies",
    "$dynamicRef",
    "$dynamicAnchor",
    "$recursiveRef",
    "$recursiveAnchor",
];
/// Longest chain of `$ref` hops followed at one value before the schema counts as cyclic.
const MAX_REF_HOPS: usize = 64;
/// Evaluations reached through a `$ref`, bounding how far a branching recursive schema can expand.
#[cfg(not(test))]
const MAX_REF_STEPS: usize = 5_000_000;
/// Same bounding path as production, reached after far fewer steps so the branching test is cheap.
#[cfg(test)]
const MAX_REF_STEPS: usize = 50_000;
/// Longest branch failure quoted inside a `oneOf`/`anyOf` message.
const MAX_REASON: usize = 120;

/// Reject a schema that is not an object or boolean, names an unknown `type`, relies on a keyword
/// this validator does not evaluate, or holds a reference that cannot be resolved locally.
pub fn check_schema(schema: &Value) -> Result<(), String> {
    let mut refs = Vec::new();
    check_at(schema, schema, "$", &mut refs)?;
    // Reference targets are checked where they are used, so an unused `$defs` entry is not visited.
    let mut seen = HashSet::new();
    while let Some(reference) = refs.pop() {
        if seen.insert(reference.clone()) {
            let target = resolve(schema, &reference)
                .ok_or_else(|| format!("{reference}: unresolvable $ref"))?;
            check_at(schema, target, &reference, &mut refs)?;
        }
    }
    Ok(())
}

fn check_at(
    root: &Value,
    schema: &Value,
    path: &str,
    refs: &mut Vec<String>,
) -> Result<(), String> {
    let object = match schema {
        Value::Bool(_) => return Ok(()),
        Value::Object(object) => object,
        _ => return Err(format!("{path}: a schema must be a JSON object or boolean")),
    };
    if let Some(keyword) = UNSUPPORTED.iter().find(|k| object.contains_key(**k)) {
        return Err(format!("{path}: unsupported keyword {keyword:?}"));
    }
    if let Some(reference) = object.get("$ref") {
        let reference = reference
            .as_str()
            .ok_or_else(|| format!("{path}: `$ref` must be a string"))?;
        if !reference.starts_with('#') {
            return Err(format!(
                "{path}: remote $ref {reference:?} is not supported; use a local #/… pointer"
            ));
        }
        check_ref_chain(root, reference, path)?;
        refs.push(reference.to_owned());
    }
    for kind in type_names(object.get("type")).map_err(|why| format!("{path}: {why}"))? {
        if !TYPES.contains(&kind) {
            return Err(format!("{path}: unknown type {kind:?}"));
        }
    }
    if let Some(Value::Array(names)) = object.get("type")
        && names.is_empty()
    {
        return Err(format!("{path}: `type` must not be an empty array"));
    }
    if let Some(required) = object.get("required") {
        let ok = required
            .as_array()
            .is_some_and(|names| names.iter().all(Value::is_string));
        if !ok {
            return Err(format!("{path}: `required` must be an array of strings"));
        }
    }
    if let Some(properties) = object.get("properties") {
        let properties = properties
            .as_object()
            .ok_or_else(|| format!("{path}: `properties` must be an object"))?;
        for (name, sub) in properties {
            check_at(root, sub, &format!("{path}.{name}"), refs)?;
        }
    }
    if let Some(patterns) = object.get("patternProperties") {
        let patterns = patterns
            .as_object()
            .ok_or_else(|| format!("{path}: `patternProperties` must be an object"))?;
        for (pattern, sub) in patterns {
            if Regex::new(pattern).is_err() {
                return Err(format!("{path}: invalid pattern {pattern:?}"));
            }
            check_at(root, sub, &format!("{path}.{pattern}"), refs)?;
        }
    }
    if let Some(deps) = object.get("dependentRequired") {
        let deps = deps
            .as_object()
            .ok_or_else(|| format!("{path}: `dependentRequired` must be an object"))?;
        if !deps.values().all(|names| {
            names
                .as_array()
                .is_some_and(|n| n.iter().all(Value::is_string))
        }) {
            return Err(format!(
                "{path}: `dependentRequired` must map names to arrays of strings"
            ));
        }
    }
    for key in ["$defs", "definitions"] {
        if object.get(key).is_some_and(|v| !v.is_object()) {
            return Err(format!("{path}: `{key}` must be an object"));
        }
    }
    for key in [
        "additionalProperties",
        "propertyNames",
        "additionalItems",
        "contains",
        "not",
        "if",
        "then",
        "else",
    ] {
        if let Some(sub) = object.get(key) {
            check_at(root, sub, &format!("{path}.{key}"), refs)?;
        }
    }
    match object.get("items") {
        Some(Value::Array(list)) => {
            for (i, sub) in list.iter().enumerate() {
                check_at(root, sub, &format!("{path}[{i}]"), refs)?;
            }
        }
        Some(sub) => check_at(root, sub, &format!("{path}[]"), refs)?,
        None => {}
    }
    for key in ["prefixItems", "allOf", "anyOf", "oneOf"] {
        if let Some(list) = object.get(key) {
            let list = list
                .as_array()
                .filter(|l| !l.is_empty())
                .ok_or_else(|| format!("{path}: `{key}` must be a non-empty array of schemas"))?;
            for (i, sub) in list.iter().enumerate() {
                check_at(root, sub, &format!("{path}.{key}[{i}]"), refs)?;
            }
        }
    }
    if let Some(values) = object.get("enum")
        && !values.as_array().is_some_and(|v| !v.is_empty())
    {
        return Err(format!("{path}: `enum` must be a non-empty array"));
    }
    for key in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        if object.get(key).is_some_and(|v| !v.is_number()) {
            return Err(format!("{path}: `{key}` must be a number"));
        }
    }
    if let Some(step) = object.get("multipleOf")
        && !step.as_f64().is_some_and(|s| s > 0.0)
    {
        return Err(format!(
            "{path}: `multipleOf` must be a number greater than 0"
        ));
    }
    for key in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minContains",
        "maxContains",
        "minProperties",
        "maxProperties",
    ] {
        if object.get(key).is_some_and(|v| v.as_u64().is_none()) {
            return Err(format!("{path}: `{key}` must be a non-negative integer"));
        }
    }
    if object.get("uniqueItems").is_some_and(|v| !v.is_boolean()) {
        return Err(format!("{path}: `uniqueItems` must be a boolean"));
    }
    if let Some(pattern) = object.get("pattern") {
        let pattern = pattern
            .as_str()
            .ok_or_else(|| format!("{path}: `pattern` must be a string"))?;
        if Regex::new(pattern).is_err() {
            return Err(format!("{path}: invalid pattern {pattern:?}"));
        }
    }
    if object.get("format").is_some_and(|v| !v.is_string()) {
        return Err(format!("{path}: `format` must be a string"));
    }
    Ok(())
}

/// Reject a `$ref` chain that loops without reaching a schema, e.g. `{"$ref": "#"}`.
fn check_ref_chain(root: &Value, reference: &str, path: &str) -> Result<(), String> {
    let mut current = reference;
    for _ in 0..=MAX_REF_HOPS {
        let target = resolve(root, current)
            .ok_or_else(|| format!("{path}: unresolvable $ref {current:?}"))?;
        match target.get("$ref").and_then(Value::as_str) {
            Some(next) => current = next,
            None => return Ok(()),
        }
    }
    Err(format!(
        "{path}: $ref {reference:?} never reaches a schema (cyclic)"
    ))
}

/// Resolve a local `#…` reference against `root`, honouring JSON Pointer escapes.
fn resolve<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = percent_decode(reference.strip_prefix('#')?)?;
    if pointer.is_empty() {
        return Some(root);
    }
    let mut node = root;
    for raw in pointer.strip_prefix('/')?.split('/') {
        let token = raw.replace("~1", "/").replace("~0", "~");
        node = match node {
            Value::Object(map) => map.get(&token)?,
            Value::Array(items)
                if !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit()) =>
            {
                items.get(token.parse::<usize>().ok()?)?
            }
            _ => return None,
        };
    }
    Some(node)
}

fn percent_decode(text: &str) -> Option<String> {
    if !text.contains('%') {
        return Some(text.to_owned());
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn type_names(value: Option<&Value>) -> Result<Vec<&str>, &'static str> {
    match value {
        None => Ok(vec![]),
        Some(Value::String(name)) => Ok(vec![name]),
        Some(Value::Array(names)) => names
            .iter()
            .map(|n| n.as_str().ok_or("`type` entries must be strings"))
            .collect(),
        Some(_) => Err("`type` must be a string or an array of strings"),
    }
}

/// Per-validation state: the root for `$ref`, compiled patterns, and the `$ref` evaluation budget.
struct Cx<'a> {
    root: &'a Value,
    budget: Cell<usize>,
    patterns: RefCell<HashMap<String, Regex>>,
}

/// Check `value` against `schema`; the error names the first offending location.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    match validate_all(schema, value, 1).into_iter().next() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Up to `max` distinct errors, in evaluation order, for retry feedback.
pub fn validate_all(schema: &Value, value: &Value, max: usize) -> Vec<String> {
    let cx = Cx {
        root: schema,
        budget: Cell::new(MAX_REF_STEPS),
        patterns: RefCell::new(HashMap::new()),
    };
    let mut errors = Vec::new();
    eval(&cx, schema, value, "$", 0, &mut errors);
    let mut seen = HashSet::new();
    errors.retain(|error| seen.insert(error.clone()));
    errors.truncate(max);
    errors
}

/// Evaluate `schema` at `value`, appending every failure. `hops` counts the `$ref`s followed at this
/// same value position; it resets to zero whenever the evaluation descends into a child value.
fn eval(cx: &Cx, schema: &Value, value: &Value, path: &str, hops: usize, out: &mut Vec<String>) {
    if hops > 0 {
        let left = cx.budget.get();
        if left == 0 {
            out.push(format!("{path}: schema is too expensive to evaluate"));
            return;
        }
        cx.budget.set(left - 1);
    }
    let schema = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            out.push(format!("{path}: no value is allowed here"));
            return;
        }
        Value::Object(object) => object,
        _ => return,
    };
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if hops >= MAX_REF_HOPS {
            out.push(format!(
                "{path}: $ref chain deeper than {MAX_REF_HOPS} (cyclic schema)"
            ));
            return;
        }
        match resolve(cx.root, reference) {
            Some(target) => eval(cx, target, value, path, hops + 1, out),
            None => out.push(format!("{path}: unresolvable $ref {reference:?}")),
        }
    }
    let kinds = type_names(schema.get("type")).unwrap_or_default();
    if !kinds.is_empty() && !kinds.iter().any(|kind| is_type(kind, value)) {
        out.push(format!(
            "{path}: expected {}, got {}",
            kinds.join(" or "),
            type_of(value)
        ));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.iter().any(|candidate| same(candidate, value))
    {
        out.push(format!(
            "{path}: {value} is not one of {}",
            Value::Array(allowed.clone())
        ));
    }
    if let Some(expected) = schema.get("const")
        && !same(expected, value)
    {
        out.push(format!("{path}: must equal {expected}"));
    }
    match value {
        Value::Number(_) => check_number(schema, value, path, out),
        Value::String(text) => check_string(cx, schema, text, path, out),
        Value::Object(map) => check_object(cx, schema, map, path, out),
        Value::Array(items) => check_array(cx, schema, items, path, out),
        _ => {}
    }
    check_combinators(cx, schema, value, path, hops, out);
}

fn check_number(schema: &Map<String, Value>, value: &Value, path: &str, out: &mut Vec<String>) {
    let Some(n) = value.as_f64() else { return };
    let bound = |key: &str| schema.get(key).and_then(Value::as_f64);
    if let Some(min) = bound("minimum")
        && n < min
    {
        out.push(format!("{path}: {n} is below the minimum {min}"));
    }
    if let Some(max) = bound("maximum")
        && n > max
    {
        out.push(format!("{path}: {n} is above the maximum {max}"));
    }
    if let Some(min) = bound("exclusiveMinimum")
        && n <= min
    {
        out.push(format!(
            "{path}: {n} is not above the exclusive minimum {min}"
        ));
    }
    if let Some(max) = bound("exclusiveMaximum")
        && n >= max
    {
        out.push(format!(
            "{path}: {n} is not below the exclusive maximum {max}"
        ));
    }
    if let Some(step) = bound("multipleOf")
        && !is_multiple(n, step)
    {
        out.push(format!("{path}: {n} is not a multiple of {step}"));
    }
}

fn is_multiple(n: f64, step: f64) -> bool {
    if step <= 0.0 || !step.is_finite() {
        return false;
    }
    if n.fract() == 0.0 && step.fract() == 0.0 {
        return n % step == 0.0;
    }
    let quotient = n / step;
    (quotient - quotient.round()).abs() <= 1e-9 * quotient.abs().max(1.0)
}

fn check_string(
    cx: &Cx,
    schema: &Map<String, Value>,
    text: &str,
    path: &str,
    out: &mut Vec<String>,
) {
    let length = text.chars().count() as u64;
    if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
        && length < min
    {
        out.push(format!(
            "{path}: has {length} characters, needs at least {min}"
        ));
    }
    if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
        && length > max
    {
        out.push(format!(
            "{path}: has {length} characters, allows at most {max}"
        ));
    }
    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
        match pattern_matches(cx, pattern, text) {
            Ok(true) => {}
            Ok(false) => out.push(format!("{path}: does not match pattern {pattern:?}")),
            Err(why) => out.push(format!("{path}: {why}")),
        }
    }
    if let Some(format_name) = schema.get("format").and_then(Value::as_str)
        && format_ok(format_name, text) == Some(false)
    {
        out.push(format!("{path}: is not a valid {format_name}"));
    }
}

fn pattern_matches(cx: &Cx, pattern: &str, text: &str) -> Result<bool, String> {
    let mut cache = cx.patterns.borrow_mut();
    if !cache.contains_key(pattern) {
        let regex =
            Regex::new(pattern).map_err(|why| format!("invalid pattern {pattern:?}: {why}"))?;
        cache.insert(pattern.to_owned(), regex);
    }
    Ok(cache.get(pattern).is_some_and(|regex| regex.is_match(text)))
}

fn check_object(
    cx: &Cx,
    schema: &Map<String, Value>,
    map: &Map<String, Value>,
    path: &str,
    out: &mut Vec<String>,
) {
    for name in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !map.contains_key(name) {
            out.push(format!("{path}: missing required property {name:?}"));
        }
    }
    let count = map.len() as u64;
    if let Some(min) = schema.get("minProperties").and_then(Value::as_u64)
        && count < min
    {
        out.push(format!(
            "{path}: has {count} properties, needs at least {min}"
        ));
    }
    if let Some(max) = schema.get("maxProperties").and_then(Value::as_u64)
        && count > max
    {
        out.push(format!(
            "{path}: has {count} properties, allows at most {max}"
        ));
    }
    if let Some(deps) = schema.get("dependentRequired").and_then(Value::as_object) {
        for (name, needed) in deps {
            if !map.contains_key(name) {
                continue;
            }
            for dep in needed
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !map.contains_key(dep) {
                    out.push(format!("{path}: property {name:?} requires {dep:?}"));
                }
            }
        }
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    let patterns = schema.get("patternProperties").and_then(Value::as_object);
    for (name, value) in map {
        let here = format!("{path}.{name}");
        let mut matched = false;
        if let Some(sub) = properties.and_then(|p| p.get(name)) {
            matched = true;
            eval(cx, sub, value, &here, 0, out);
        }
        if let Some(patterns) = patterns {
            for (pattern, sub) in patterns {
                match pattern_matches(cx, pattern, name) {
                    Ok(true) => {
                        matched = true;
                        eval(cx, sub, value, &here, 0, out);
                    }
                    Ok(false) => {}
                    Err(why) => out.push(format!("{path}: {why}")),
                }
            }
        }
        if !matched {
            match schema.get("additionalProperties") {
                Some(Value::Bool(false)) => {
                    out.push(format!("{path}: unexpected property {name:?}"));
                }
                Some(extra) => eval(cx, extra, value, &here, 0, out),
                None => {}
            }
        }
        if let Some(names) = schema.get("propertyNames") {
            let key = Value::String(name.clone());
            eval(cx, names, &key, &format!("{path}[{name:?}]"), 0, out);
        }
    }
}

fn check_array(
    cx: &Cx,
    schema: &Map<String, Value>,
    items: &[Value],
    path: &str,
    out: &mut Vec<String>,
) {
    let len = items.len() as u64;
    if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
        && len < min
    {
        out.push(format!("{path}: needs at least {min} items, has {len}"));
    }
    if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
        && len > max
    {
        out.push(format!("{path}: allows at most {max} items, has {len}"));
    }
    if schema
        .get("uniqueItems")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        for (j, later) in items.iter().enumerate() {
            if let Some(i) = items[..j].iter().position(|earlier| same(earlier, later)) {
                out.push(format!("{path}[{j}]: duplicates item {i}"));
            }
        }
    }
    let no_items: &[Value] = &[];
    let (positional, rest): (&[Value], Option<&Value>) =
        match (schema.get("prefixItems"), schema.get("items")) {
            (Some(Value::Array(prefix)), rest) => (prefix.as_slice(), rest),
            (_, Some(Value::Array(tuple))) => (tuple.as_slice(), schema.get("additionalItems")),
            (_, rest) => (no_items, rest),
        };
    for (i, item) in items.iter().enumerate() {
        if let Some(sub) = positional.get(i).or(rest) {
            eval(cx, sub, item, &format!("{path}[{i}]"), 0, out);
        }
    }
    if let Some(contains) = schema.get("contains") {
        let matching = items
            .iter()
            .filter(|item| failures(cx, contains, item, path, 0).is_empty())
            .count() as u64;
        let min = schema
            .get("minContains")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        if matching < min {
            out.push(format!(
                "{path}: {matching} items match `contains`, needs at least {min}"
            ));
        }
        if let Some(max) = schema.get("maxContains").and_then(Value::as_u64)
            && matching > max
        {
            out.push(format!(
                "{path}: {matching} items match `contains`, allows at most {max}"
            ));
        }
    }
}

fn check_combinators(
    cx: &Cx,
    schema: &Map<String, Value>,
    value: &Value,
    path: &str,
    hops: usize,
    out: &mut Vec<String>,
) {
    for branch in schema
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        eval(cx, branch, value, path, hops, out);
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        let mut reasons = Vec::new();
        let mut matched = false;
        for branch in branches {
            let errors = failures(cx, branch, value, path, hops);
            if errors.is_empty() {
                matched = true;
                break;
            }
            reasons.push(brief(&errors));
        }
        if !matched {
            out.push(format!(
                "{path}: matches none of the anyOf branches ({})",
                summarize(&reasons)
            ));
        }
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let mut reasons = Vec::new();
        let mut passed = 0usize;
        for branch in branches {
            let errors = failures(cx, branch, value, path, hops);
            if errors.is_empty() {
                passed += 1;
            } else {
                reasons.push(brief(&errors));
            }
        }
        match passed {
            1 => {}
            0 => out.push(format!(
                "{path}: matches none of the oneOf branches ({})",
                summarize(&reasons)
            )),
            n => out.push(format!(
                "{path}: matches {n} of the oneOf branches, exactly one is allowed"
            )),
        }
    }
    if let Some(negated) = schema.get("not")
        && failures(cx, negated, value, path, hops).is_empty()
    {
        out.push(format!("{path}: must not match the `not` schema"));
    }
    if let Some(condition) = schema.get("if") {
        let branch = if failures(cx, condition, value, path, hops).is_empty() {
            "then"
        } else {
            "else"
        };
        if let Some(sub) = schema.get(branch) {
            eval(cx, sub, value, path, hops, out);
        }
    }
}

/// Every failure of `schema` at `value`; empty means the value matches.
fn failures(cx: &Cx, schema: &Value, value: &Value, path: &str, hops: usize) -> Vec<String> {
    let mut errors = Vec::new();
    eval(cx, schema, value, path, hops, &mut errors);
    errors
}

/// The first failure of a rejected branch, shortened so a combinator message stays bounded.
fn brief(errors: &[String]) -> String {
    errors
        .first()
        .map(|error| error.chars().take(MAX_REASON).collect())
        .unwrap_or_default()
}

/// Join the first few branch reasons and count the rest.
fn summarize(reasons: &[String]) -> String {
    const SHOWN: usize = 4;
    let mut text = reasons
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("; ");
    if reasons.len() > SHOWN {
        text.push_str(&format!("; and {} more", reasons.len() - SHOWN));
    }
    text
}

/// JSON equality for `enum`, `const` and `uniqueItems`: `1` and `1.0` are the same number.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) if x.is_f64() || y.is_f64() => {
            a.as_f64() == b.as_f64()
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, v)| y.get(key).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

/// `Some(valid)` for a format this validator knows; `None` for an annotation-only format.
fn format_ok(name: &str, text: &str) -> Option<bool> {
    Some(match name {
        "date-time" => date_time(text),
        "date" => date(text),
        "time" => time(text),
        "email" => email(text),
        "uri" => uri(text),
        "uuid" => uuid(text),
        "ipv4" => text.parse::<Ipv4Addr>().is_ok(),
        "ipv6" => text.parse::<Ipv6Addr>().is_ok(),
        "hostname" => hostname(text),
        _ => return None,
    })
}

static DATE: OnceLock<Regex> = OnceLock::new();
static TIME: OnceLock<Regex> = OnceLock::new();
static URI: OnceLock<Regex> = OnceLock::new();

fn static_regex(cell: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(source).expect("static format pattern is valid"))
}

fn date_parts(text: &str) -> Option<(u32, u32, u32)> {
    let caps = static_regex(&DATE, r"^([0-9]{4})-([0-9]{2})-([0-9]{2})$").captures(text)?;
    let part = |i: usize| caps.get(i)?.as_str().parse::<u32>().ok();
    let (year, month, day) = (part(1)?, part(2)?, part(3)?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let longest = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    (1..=longest).contains(&day).then_some((year, month, day))
}

fn date(text: &str) -> bool {
    date_parts(text).is_some()
}

/// RFC 3339 `full-time`: a clock time with a mandatory offset (`Z` or `±hh:mm`).
fn time(text: &str) -> bool {
    let Some(caps) = static_regex(
        &TIME,
        r"^([0-9]{2}):([0-9]{2}):([0-9]{2})(?:\.[0-9]+)?(?:[Zz]|[+-]([0-9]{2}):([0-9]{2}))$",
    )
    .captures(text) else {
        return false;
    };
    let num = |i: usize| caps.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
    let clock_ok = num(1).is_some_and(|h| h <= 23)
        && num(2).is_some_and(|m| m <= 59)
        // 60 is a leap second.
        && num(3).is_some_and(|s| s <= 60);
    let offset_ok = match (num(4), num(5)) {
        (Some(h), Some(m)) => h <= 23 && m <= 59,
        _ => true,
    };
    clock_ok && offset_ok
}

fn date_time(text: &str) -> bool {
    match (text.get(..10), text.get(10..11), text.get(11..)) {
        (Some(day), Some("T" | "t"), Some(clock)) => date(day) && time(clock),
        _ => false,
    }
}

fn email(text: &str) -> bool {
    let Some((local, domain)) = text.rsplit_once('@') else {
        return false;
    };
    let atext = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~.".contains(c);
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && local.chars().all(atext)
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..");
    let domain_ok = hostname(domain)
        || domain
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .is_some_and(|ip| ip.parse::<IpAddr>().is_ok());
    local_ok && domain_ok
}

/// RFC 1123 hostname: dot-separated labels of letters, digits and inner hyphens.
fn hostname(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 253
        && text.split('.').all(|label| {
            let bytes = label.as_bytes();
            (1..=63).contains(&bytes.len())
                && bytes[0].is_ascii_alphanumeric()
                && bytes[bytes.len() - 1].is_ascii_alphanumeric()
                && bytes
                    .iter()
                    .all(|c| c.is_ascii_alphanumeric() || *c == b'-')
        })
}

/// RFC 3986 absolute URI: a scheme, a colon, then characters allowed in a URI.
fn uri(text: &str) -> bool {
    static_regex(
        &URI,
        r#"^[A-Za-z][A-Za-z0-9+.\-]*:[^\x00-\x20\x7F<>"\\^`{|}]*$"#,
    )
    .is_match(text)
}

fn uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                *c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn is_type(kind: &str, value: &Value) -> bool {
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        // JSON has one number type: 3.0 is an integer.
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests;
