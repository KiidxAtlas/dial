use serde::Serialize;
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::ops::Range;

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 128;

struct Node {
    span: Range<usize>,
    kind: Kind,
}

enum Kind {
    Scalar(Value),
    Array(Vec<Node>),
    Object(Object),
}

struct Property {
    name: String,
    key_span: Range<usize>,
    value: Node,
    shadowed: bool,
    duplicate: bool,
}

struct Object {
    properties: Vec<Property>,
    unique_count: usize,
}

impl Node {
    fn equals(&self, value: &Value) -> bool {
        match (&self.kind, value) {
            (Kind::Scalar(old), next) => old == next,
            (Kind::Array(old), Value::Array(next)) => {
                old.len() == next.len() && old.iter().zip(next).all(|(old, next)| old.equals(next))
            }
            (Kind::Object(old), Value::Object(next)) => {
                old.unique_count == next.len()
                    && old
                        .properties
                        .iter()
                        .filter(|p| !p.shadowed)
                        .all(|p| next.get(&p.name).is_some_and(|next| p.value.equals(next)))
            }
            _ => false,
        }
    }

    fn object(&self) -> Option<&Object> {
        match &self.kind {
            Kind::Object(object) => Some(object),
            _ => None,
        }
    }
    fn into_value(self) -> Value {
        match self.kind {
            Kind::Scalar(value) => value,
            Kind::Array(nodes) => Value::Array(nodes.into_iter().map(Node::into_value).collect()),
            Kind::Object(object) => Value::Object(
                object
                    .properties
                    .into_iter()
                    .filter(|property| !property.shadowed)
                    .map(|property| (property.name, property.value.into_value()))
                    .collect(),
            ),
        }
    }
}

impl Object {
    fn get(&self, name: &str) -> Option<&Node> {
        self.properties
            .iter()
            .find(|p| !p.shadowed && p.name == name)
            .map(|p| &p.value)
    }

    fn reject_controlled_duplicates(&self, names: &[&str], context: &str) -> Result<(), String> {
        for property in &self.properties {
            if property.duplicate && names.contains(&property.name.as_str()) {
                return Err(format!(
                    "ambiguous duplicate controlled property {:?} in {context}",
                    property.name
                ));
            }
        }
        Ok(())
    }
}

struct Parser<'a> {
    raw: &'a str,
    position: usize,
}

impl<'a> Parser<'a> {
    fn error(&self, message: &str) -> String {
        format!(
            "invalid original notebook JSON at byte {}: {message}",
            self.position
        )
    }

    fn whitespace(&mut self) {
        while matches!(
            self.raw.as_bytes().get(self.position),
            Some(b' ' | b'\t' | b'\r' | b'\n')
        ) {
            self.position += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.raw.as_bytes().get(self.position) == Some(&byte) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn string_span(&mut self) -> Result<Range<usize>, String> {
        let start = self.position;
        if !self.consume(b'"') {
            return Err(self.error("expected a string"));
        }
        loop {
            let byte = *self
                .raw
                .as_bytes()
                .get(self.position)
                .ok_or_else(|| self.error("unterminated string"))?;
            self.position += 1;
            match byte {
                b'"' => return Ok(start..self.position),
                b'\\' => {
                    let escape = *self
                        .raw
                        .as_bytes()
                        .get(self.position)
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    self.position += 1;
                    match escape {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                        b'u' => {
                            for _ in 0..4 {
                                if !self
                                    .raw
                                    .as_bytes()
                                    .get(self.position)
                                    .is_some_and(u8::is_ascii_hexdigit)
                                {
                                    return Err(self.error("invalid Unicode escape"));
                                }
                                self.position += 1;
                            }
                        }
                        _ => return Err(self.error("invalid string escape")),
                    }
                }
                0..=31 => return Err(self.error("unescaped control character")),
                _ => {}
            }
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.position;
        while self
            .raw
            .as_bytes()
            .get(self.position)
            .is_some_and(u8::is_ascii_digit)
        {
            self.position += 1;
        }
        self.position - start
    }

    fn number(&mut self) -> Result<(), String> {
        self.consume(b'-');
        if !self.consume(b'0') {
            if !matches!(self.raw.as_bytes().get(self.position), Some(b'1'..=b'9')) {
                return Err(self.error("invalid number"));
            }
            self.digits();
        }
        if self.consume(b'.') && self.digits() == 0 {
            return Err(self.error("missing fraction digits"));
        }
        if self.consume(b'e') || self.consume(b'E') {
            if !self.consume(b'+') {
                self.consume(b'-');
            }
            if self.digits() == 0 {
                return Err(self.error("missing exponent digits"));
            }
        }
        Ok(())
    }

    fn node(&mut self, depth: usize) -> Result<Node, String> {
        if depth > MAX_DEPTH {
            return Err(self.error("JSON nesting exceeds the 128-level carrier limit"));
        }
        self.whitespace();
        let start = self.position;
        let byte = *self
            .raw
            .as_bytes()
            .get(start)
            .ok_or_else(|| self.error("expected a value"))?;
        let kind = match byte {
            b'{' => {
                self.position += 1;
                self.whitespace();
                let mut properties = Vec::new();
                if !self.consume(b'}') {
                    loop {
                        let key_span = self.string_span()?;
                        let name = serde_json::from_str::<String>(&self.raw[key_span.clone()])
                            .map_err(|e| format!("invalid original notebook JSON string: {e}"))?;
                        self.whitespace();
                        if !self.consume(b':') {
                            return Err(self.error("expected ':' after property name"));
                        }
                        let value = self.node(depth + 1)?;
                        properties.push(Property {
                            name,
                            key_span,
                            value,
                            shadowed: false,
                            duplicate: false,
                        });
                        self.whitespace();
                        if self.consume(b'}') {
                            break;
                        }
                        if !self.consume(b',') {
                            return Err(self.error("expected ',' or '}'"));
                        }
                        self.whitespace();
                    }
                }
                // Sorting indices, rather than copying keys, records last-key-wins
                // semantics while keeping every original property in source order.
                let mut order: Vec<usize> = (0..properties.len()).collect();
                order.sort_unstable_by(|&a, &b| {
                    properties[a].name.cmp(&properties[b].name).then(a.cmp(&b))
                });
                let mut unique_count = 0;
                let mut first = 0;
                while first < order.len() {
                    let mut end = first + 1;
                    while end < order.len()
                        && properties[order[first]].name == properties[order[end]].name
                    {
                        end += 1;
                    }
                    unique_count += 1;
                    for &index in &order[first..end] {
                        properties[index].duplicate = end - first > 1;
                        properties[index].shadowed = index != order[end - 1];
                    }
                    first = end;
                }
                Kind::Object(Object {
                    properties,
                    unique_count,
                })
            }
            b'[' => {
                self.position += 1;
                self.whitespace();
                let mut values = Vec::new();
                if !self.consume(b']') {
                    loop {
                        values.push(self.node(depth + 1)?);
                        self.whitespace();
                        if self.consume(b']') {
                            break;
                        }
                        if !self.consume(b',') {
                            return Err(self.error("expected ',' or ']'"));
                        }
                        self.whitespace();
                    }
                }
                Kind::Array(values)
            }
            _ => {
                match byte {
                    b'"' => {
                        self.string_span()?;
                    }
                    b'-' | b'0'..=b'9' => self.number()?,
                    b'n' | b't' | b'f' => {
                        let literal = match byte {
                            b'n' => "null",
                            b't' => "true",
                            _ => "false",
                        };
                        if !self.raw[self.position..].starts_with(literal) {
                            return Err(self.error("invalid literal"));
                        }
                        self.position += literal.len();
                    }
                    _ => return Err(self.error("expected a JSON value")),
                }
                let value = serde_json::from_str::<Value>(&self.raw[start..self.position])
                    .map_err(|e| {
                        format!("invalid original notebook JSON scalar at byte {start}: {e}")
                    })?;
                Kind::Scalar(value)
            }
        };
        Ok(Node {
            span: start..self.position,
            kind,
        })
    }
}

struct Output(Vec<u8>);

impl Output {
    fn append(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > MAX_BYTES.saturating_sub(self.0.len()) {
            return Err("encoded notebook JSON exceeds the 8 MiB carrier limit".into());
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    fn canonical(&mut self, value: &Value) -> Result<(), String> {
        value
            .serialize(&mut serde_json::Serializer::new(&mut *self))
            .map_err(|e| format!("cannot encode notebook JSON: {e}"))
    }
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.append(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BudgetSink<'a>(&'a mut usize);

impl Write for BudgetSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        *self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "edited notebook JSON exceeds the 8 MiB carrier budget",
            )
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn validate_value(value: &Value, depth: usize, budget: &mut usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("edited notebook JSON nesting exceeds the 128-level carrier limit".into());
    }
    let weight = if value.is_number() {
        0
    } else {
        1usize.saturating_add(value.as_str().map_or(0, str::len))
    };
    *budget = budget
        .checked_sub(weight)
        .ok_or("edited notebook JSON exceeds the 8 MiB carrier budget")?;
    match value {
        Value::Number(_) => {
            value
                .serialize(&mut serde_json::Serializer::new(BudgetSink(budget)))
                .map_err(|e| {
                    format!("edited notebook numeric token exceeds its carrier budget: {e}")
                })?;
        }
        Value::Array(values) => {
            for value in values {
                validate_value(value, depth + 1, budget)?;
            }
        }
        Value::Object(values) => {
            for (name, value) in values {
                *budget = budget
                    .checked_sub(name.len())
                    .ok_or("edited notebook JSON exceeds the 8 MiB carrier budget")?;
                validate_value(value, depth + 1, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn cell_id(node: &Node) -> Option<&str> {
    match &node.object()?.get("id")?.kind {
        Kind::Scalar(Value::String(id)) if !id.is_empty() => Some(id),
        _ => None,
    }
}

fn value_cell_id(value: &Value) -> Option<&str> {
    value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

struct HashSink(DefaultHasher);

impl Write for HashSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn scalar_fingerprint(value: &Value) -> Result<u64, String> {
    let mut sink = HashSink(DefaultHasher::new());
    value
        .serialize(&mut serde_json::Serializer::new(&mut sink))
        .map_err(|e| format!("cannot compare notebook JSON scalar: {e}"))?;
    Ok(sink.0.finish())
}

fn field_fingerprint(name: &str, value: u64) -> u64 {
    let mut hash = DefaultHasher::new();
    name.hash(&mut hash);
    value.hash(&mut hash);
    hash.finish()
}

fn node_fingerprint(node: &Node) -> Result<u64, String> {
    let mut hash = DefaultHasher::new();
    match &node.kind {
        Kind::Scalar(value) => return scalar_fingerprint(value),
        Kind::Array(values) => {
            1u8.hash(&mut hash);
            values.len().hash(&mut hash);
            for value in values {
                node_fingerprint(value)?.hash(&mut hash);
            }
        }
        Kind::Object(object) => {
            2u8.hash(&mut hash);
            object.unique_count.hash(&mut hash);
            let mut fields = 0u64;
            for property in object.properties.iter().filter(|p| !p.shadowed) {
                fields = fields.wrapping_add(field_fingerprint(
                    &property.name,
                    node_fingerprint(&property.value)?,
                ));
            }
            fields.hash(&mut hash);
        }
    }
    Ok(hash.finish())
}

fn value_fingerprint(value: &Value) -> Result<u64, String> {
    let mut hash = DefaultHasher::new();
    match value {
        Value::Array(values) => {
            1u8.hash(&mut hash);
            values.len().hash(&mut hash);
            for value in values {
                value_fingerprint(value)?.hash(&mut hash);
            }
        }
        Value::Object(object) => {
            2u8.hash(&mut hash);
            object.len().hash(&mut hash);
            let mut fields = 0u64;
            for (name, value) in object {
                fields = fields.wrapping_add(field_fingerprint(name, value_fingerprint(value)?));
            }
            fields.hash(&mut hash);
        }
        _ => return scalar_fingerprint(value),
    }
    Ok(hash.finish())
}

fn origins(
    old: &[Node],
    next: &[Value],
    supplied: Option<&[Option<usize>]>,
) -> Result<Vec<Option<usize>>, String> {
    let mut used = HashSet::new();
    if let Some(supplied) = supplied {
        if supplied.len() != next.len() {
            return Err("cell origin count does not match the edited cells array".into());
        }
        for &origin in supplied.iter().flatten() {
            if origin >= old.len() {
                return Err(format!(
                    "cell origin {origin} is outside the original cells array"
                ));
            }
            if !used.insert(origin) {
                return Err(format!("cell origin {origin} is reused more than once"));
            }
        }
        return Ok(supplied.to_vec());
    }

    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, cell) in old.iter().enumerate() {
        if let Some(id) = cell_id(cell) {
            by_id.entry(id).or_default().push(index);
        }
    }
    let mut result = vec![None; next.len()];
    for (index, cell) in next.iter().enumerate() {
        if let Some(candidates) = value_cell_id(cell).and_then(|id| by_id.get(id)) {
            if candidates.len() != 1 || !used.insert(candidates[0]) {
                return Err(format!(
                    "ambiguous original cell ID for edited cell {index}; exact cell origins are required"
                ));
            }
            result[index] = Some(candidates[0]);
        }
    }
    // Fingerprints are an index only: every candidate still receives a full
    // semantic comparison. This avoids an all-cells quadratic search.
    let mut by_value: HashMap<u64, Vec<usize>> = HashMap::new();
    if result.iter().any(Option::is_none) {
        for (index, cell) in old.iter().enumerate() {
            by_value
                .entry(node_fingerprint(cell)?)
                .or_default()
                .push(index);
        }
    }
    for (index, cell) in next.iter().enumerate() {
        if result[index].is_some() {
            continue;
        }
        let mut candidate = None;
        if let Some(candidates) = by_value.get(&value_fingerprint(cell)?) {
            for &original in candidates {
                if old[original].equals(cell) {
                    if candidate.is_some() {
                        return Err(format!(
                            "ambiguous unchanged cell {index}; exact cell origins are required"
                        ));
                    }
                    candidate = Some(original);
                }
            }
        }
        if let Some(original) = candidate {
            if !used.insert(original) {
                return Err(format!(
                    "edited cell {index} reuses an original cell ambiguously; exact cell origins are required"
                ));
            }
            result[index] = Some(original);
        }
    }
    if used.len() < old.len() {
        for (index, cell) in next.iter().enumerate() {
            if result[index].is_none() && value_cell_id(cell).is_none() {
                return Err(format!(
                    "cannot authenticate changed ID-less cell {index}; exact cell origins are required to preserve its raw metadata"
                ));
            }
        }
    }
    Ok(result)
}

struct Encoder<'a> {
    raw: &'a str,
    output: Output,
}

impl Encoder<'_> {
    fn raw(&mut self, span: Range<usize>) -> Result<(), String> {
        self.output.append(self.raw[span].as_bytes())
    }

    fn node(&mut self, old: &Node, next: &Value) -> Result<(), String> {
        if old.equals(next) {
            return self.raw(old.span.clone());
        }
        match (&old.kind, next) {
            (Kind::Object(object), Value::Object(next)) => self.object(old, object, next, None),
            (Kind::Array(old), Value::Array(next)) => {
                self.output.append(b"[")?;
                for (index, next) in next.iter().enumerate() {
                    if index != 0 {
                        self.output.append(b",")?;
                    }
                    if let Some(old) = old.get(index) {
                        self.node(old, next)?;
                    } else {
                        self.output.canonical(next)?;
                    }
                }
                self.output.append(b"]")
            }
            _ => self.output.canonical(next),
        }
    }

    fn cells(&mut self, old: &Node, next: &Value, origins: &[Option<usize>]) -> Result<(), String> {
        let Kind::Array(original) = &old.kind else {
            return Err("original cells is not an array".into());
        };
        let next = next.as_array().ok_or("edited cells is not an array")?;
        if original.len() == next.len()
            && origins
                .iter()
                .enumerate()
                .all(|(index, origin)| *origin == Some(index))
        {
            // Preserve every original delimiter and inter-cell whitespace as well.
            let mut cursor = old.span.start;
            for (old, next) in original.iter().zip(next) {
                self.raw(cursor..old.span.start)?;
                self.node(old, next)?;
                cursor = old.span.end;
            }
            return self.raw(cursor..old.span.end);
        }
        self.output.append(b"[")?;
        for (index, next) in next.iter().enumerate() {
            if index != 0 {
                self.output.append(b",")?;
            }
            if let Some(origin) = origins[index] {
                self.node(&original[origin], next)?;
            } else {
                self.output.canonical(next)?;
            }
        }
        self.output.append(b"]")
    }

    fn property(
        &mut self,
        property: &Property,
        next: &Value,
        cells: Option<&[Option<usize>]>,
    ) -> Result<(), String> {
        if property.duplicate {
            return self.raw(property.value.span.clone());
        }
        if property.name == "cells"
            && let Some(origins) = cells
        {
            return self.cells(&property.value, next, origins);
        }
        self.node(&property.value, next)
    }

    fn object(
        &mut self,
        old: &Node,
        object: &Object,
        next: &serde_json::Map<String, Value>,
        cells: Option<&[Option<usize>]>,
    ) -> Result<(), String> {
        // Unchanged duplicate names retain every occurrence, including hidden
        // earlier values. There is no faithful Value representation for changing
        // or deleting one, so reject rather than silently collapsing them.
        for property in object
            .properties
            .iter()
            .filter(|p| p.duplicate && !p.shadowed)
        {
            if !next
                .get(&property.name)
                .is_some_and(|next| property.value.equals(next))
            {
                return Err(format!(
                    "cannot change or remove ambiguous duplicate JSON property {:?}",
                    property.name
                ));
            }
        }
        let same_keys = object.unique_count == next.len()
            && object.properties.iter().all(|p| next.contains_key(&p.name));
        if same_keys {
            let mut cursor = old.span.start;
            for property in &object.properties {
                self.raw(cursor..property.value.span.start)?;
                self.property(property, &next[&property.name], cells)?;
                cursor = property.value.span.end;
            }
            return self.raw(cursor..old.span.end);
        }
        self.output.append(b"{")?;
        let mut written = false;
        let names: HashSet<&str> = object.properties.iter().map(|p| p.name.as_str()).collect();
        for property in &object.properties {
            if let Some(next) = next.get(&property.name) {
                if written {
                    self.output.append(b",")?;
                }
                written = true;
                self.raw(property.key_span.start..property.value.span.start)?;
                self.property(property, next, cells)?;
            }
        }
        for (name, value) in next {
            if !names.contains(name.as_str()) {
                if written {
                    self.output.append(b",")?;
                }
                written = true;
                name.serialize(&mut serde_json::Serializer::new(&mut self.output))
                    .map_err(|e| format!("cannot encode notebook property name: {e}"))?;
                self.output.append(b":")?;
                self.output.canonical(value)?;
            }
        }
        self.output.append(b"}")
    }
}

/// Decode real JSON objects without serde's arbitrary-precision private-map sentinel collision.
pub fn parse(original_raw: &str) -> Result<Value, String> {
    if original_raw.len() > MAX_BYTES {
        return Err("notebook JSON exceeds the 8 MiB carrier limit".into());
    }
    let raw = original_raw
        .strip_prefix('\u{feff}')
        .unwrap_or(original_raw);
    let mut parser = Parser { raw, position: 0 };
    let node = parser.node(1)?;
    parser.whitespace();
    if parser.position != raw.len() {
        return Err("trailing data after notebook JSON".into());
    }
    if let Some(root) = node.object() {
        root.reject_controlled_duplicates(
            &["cells", "nbformat", "nbformat_minor"],
            "notebook root",
        )?;
        if let Some(cells) = root.get("cells")
            && let Kind::Array(cells) = &cells.kind
        {
            for cell in cells {
                if let Some(cell) = cell.object() {
                    cell.reject_controlled_duplicates(
                        &["id", "cell_type", "source", "outputs", "execution_count"],
                        "notebook cell",
                    )?;
                }
            }
        }
    }
    Ok(node.into_value())
}
// The caller enforces its tighter operation budget (4 MiB for generic edits).
// An empty original denotes a genuinely new notebook, not a parse fallback.
// Returned bytes omit the BOM; existing JSON whitespace remains byte-exact.
pub fn encode(
    document: &Value,
    original_raw: &str,
    cell_origins: Option<&[Option<usize>]>,
) -> Result<String, String> {
    let raw = original_raw
        .strip_prefix('\u{feff}')
        .unwrap_or(original_raw);
    if original_raw.len() > MAX_BYTES {
        return Err("original notebook JSON exceeds the 8 MiB carrier limit".into());
    }
    let mut budget = MAX_BYTES;
    validate_value(document, 1, &mut budget)?;
    let next = document
        .as_object()
        .ok_or("edited notebook JSON root is not an object")?;
    let next_cells = next
        .get("cells")
        .and_then(Value::as_array)
        .ok_or("edited notebook cells is not an array")?;
    if next_cells.iter().any(|cell| !cell.is_object()) {
        return Err("edited notebook cells must be objects".into());
    }
    let mut encoder = Encoder {
        raw,
        output: Output(Vec::new()),
    };
    if original_raw.is_empty() {
        if cell_origins.is_some_and(|origins| {
            origins.len() != next_cells.len() || origins.iter().any(Option::is_some)
        }) {
            return Err("a new notebook cannot have original cell origins".into());
        }
        document
            .serialize(&mut serde_json::Serializer::with_formatter(
                &mut encoder.output,
                serde_json::ser::PrettyFormatter::with_indent(b" "),
            ))
            .map_err(|e| format!("cannot encode new notebook JSON: {e}"))?;
    } else {
        let mut parser = Parser { raw, position: 0 };
        let original = parser.node(1)?;
        parser.whitespace();
        if parser.position != raw.len() {
            return Err(parser.error("trailing data after root value"));
        }
        let object = original
            .object()
            .ok_or("original notebook JSON root is not an object")?;
        object.reject_controlled_duplicates(
            &["cells", "nbformat", "nbformat_minor"],
            "notebook root",
        )?;
        let cells_node = object
            .get("cells")
            .ok_or("original notebook has no cells property")?;
        let Kind::Array(old_cells) = &cells_node.kind else {
            return Err("original notebook cells is not an array".into());
        };
        for (index, cell) in old_cells.iter().enumerate() {
            let object = cell
                .object()
                .ok_or_else(|| format!("original notebook cell {index} is not an object"))?;
            object.reject_controlled_duplicates(
                &["id", "cell_type", "source", "outputs", "execution_count"],
                &format!("cell {index}"),
            )?;
        }
        let origins = origins(old_cells, next_cells, cell_origins)?;
        encoder.raw(0..original.span.start)?;
        encoder.object(&original, object, next, Some(&origins))?;
        encoder.raw(original.span.end..raw.len())?;
    }
    String::from_utf8(encoder.output.0)
        .map_err(|e| format!("encoded notebook JSON is not UTF-8: {e}"))
}
