//! Parse the near-valid JSON that local models put in tool-call arguments.
//!
//! A model that writes a whole file into `{"content": "..."}` often leaves a raw newline or tab inside the
//! string, writes `\d` in a regex, or quotes with `'`. Strict JSON rejects all of those, the call is marked
//! malformed, and the model has to generate the entire (large) payload again. Every repair here changes only how
//! the text is *spelled*, never what it says:
//!
//!  * raw control characters inside strings are accepted as themselves;
//!  * an invalid escape (`\d`, `\s`) keeps its backslash;
//!  * single-quoted strings, unquoted keys and trailing commas are accepted;
//!  * a JSON object that was itself encoded as a JSON string is unwrapped once.
//!
//! Input that is cut off (unterminated string, missing brace) or has trailing text is refused: executing a
//! half-written call is worse than asking for it again.
use serde_json::{Map, Number, Value};

const MAX_DEPTH: usize = 64;

/// The arguments object, if `text` is one after repair. `None` means unrepairable: report it as malformed.
pub(crate) fn parse_arguments(text: &str) -> Option<Value> {
    let value = parse(text).ok()?;
    match value {
        Value::Object(_) => Some(value),
        // Some models double-encode: "{\"path\": ...}" as a JSON string.
        Value::String(inner) if inner.trim_start().starts_with('{') => {
            parse(&inner).ok().filter(Value::is_object)
        }
        _ => None,
    }
}

pub(crate) fn parse(text: &str) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Ok(value);
    }
    let mut parser = Parser {
        chars: text.chars().collect(),
        at: 0,
    };
    let value = parser.value(0)?;
    parser.space();
    if parser.at < parser.chars.len() {
        return Err(format!("unexpected trailing text at {}", parser.at));
    }
    Ok(value)
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.at += 1;
        }
    }

    fn expect(&mut self, wanted: char) -> Result<(), String> {
        self.space();
        if self.peek() == Some(wanted) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected '{wanted}' at {}", self.at))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err("nesting too deep".into());
        }
        self.space();
        match self.peek().ok_or("input ends before a value")? {
            '{' => self.object(depth),
            '[' => self.array(depth),
            quote @ ('"' | '\'') => self.string(quote).map(Value::String),
            '-' | '0'..='9' => self.number(),
            _ => self.keyword(),
        }
    }

    fn keyword(&mut self) -> Result<Value, String> {
        for (word, value) in [
            ("true", Value::Bool(true)),
            ("false", Value::Bool(false)),
            ("null", Value::Null),
        ] {
            let end = self.at + word.len();
            let matches = self
                .chars
                .get(self.at..end)
                .is_some_and(|slice| slice.iter().copied().eq(word.chars()));
            // `trueish` is not `true`.
            if matches
                && !self
                    .chars
                    .get(end)
                    .is_some_and(|c| c.is_alphanumeric() || *c == '_')
            {
                self.at = end;
                return Ok(value);
            }
        }
        Err(format!("unexpected character at {}", self.at))
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'))
        {
            self.at += 1;
        }
        let token: String = self.chars[start..self.at].iter().collect();
        serde_json::from_str::<Number>(&token)
            .map(Value::Number)
            .map_err(|_| format!("invalid number {token:?}"))
    }

    fn string(&mut self, quote: char) -> Result<String, String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let c = self
                .peek()
                .ok_or("string is not terminated (the arguments were cut off)")?;
            self.at += 1;
            match c {
                c if c == quote => return Ok(out),
                '\\' => out.push_str(&self.escape(quote)),
                // Raw newlines, tabs and other control characters are kept as the characters they are.
                c => out.push(c),
            }
        }
    }

    /// The text for the escape after a backslash; unknown escapes keep the backslash, which is what the model
    /// meant for a regex or Windows path.
    fn escape(&mut self, quote: char) -> String {
        let Some(c) = self.peek() else {
            return "\\".into();
        };
        self.at += 1;
        match c {
            'n' => "\n".into(),
            't' => "\t".into(),
            'r' => "\r".into(),
            'b' => "\u{8}".into(),
            'f' => "\u{c}".into(),
            '/' | '\\' | '"' => c.to_string(),
            c if c == quote => c.to_string(),
            'u' => self.unicode().unwrap_or_else(|| "\\u".into()),
            other => format!("\\{other}"),
        }
    }

    fn hex4(&self, from: usize) -> Option<u32> {
        let digits = self.chars.get(from..from + 4)?;
        u32::from_str_radix(&digits.iter().collect::<String>(), 16).ok()
    }

    fn unicode(&mut self) -> Option<String> {
        let high = self.hex4(self.at)?;
        let mut end = self.at + 4;
        let code = if (0xD800..0xDC00).contains(&high) {
            // A surrogate pair is written as two escapes; a lone half is not text.
            if self.chars.get(end) == Some(&'\\') && self.chars.get(end + 1) == Some(&'u') {
                let low = self
                    .hex4(end + 2)
                    .filter(|low| (0xDC00..0xE000).contains(low))?;
                end += 6;
                0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
            } else {
                return None;
            }
        } else {
            high
        };
        let ch = char::from_u32(code)?;
        self.at = end;
        Some(ch.to_string())
    }

    fn key(&mut self) -> Result<String, String> {
        self.space();
        match self.peek() {
            Some(quote @ ('"' | '\'')) => self.string(quote),
            Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = self.at;
                while self
                    .peek()
                    .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '-'))
                {
                    self.at += 1;
                }
                Ok(self.chars[start..self.at].iter().collect())
            }
            _ => Err(format!("expected an object key at {}", self.at)),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, String> {
        self.at += 1;
        let mut map = Map::new();
        loop {
            self.space();
            if self.peek() == Some('}') {
                self.at += 1;
                return Ok(Value::Object(map));
            }
            let key = self.key()?;
            self.expect(':')?;
            let value = self.value(depth + 1)?;
            map.insert(key, value);
            self.space();
            match self.peek() {
                Some(',') => self.at += 1,
                Some('}') => {}
                _ => return Err(format!("expected ',' or '}}' at {}", self.at)),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, String> {
        self.at += 1;
        let mut items = Vec::new();
        loop {
            self.space();
            if self.peek() == Some(']') {
                self.at += 1;
                return Ok(Value::Array(items));
            }
            items.push(self.value(depth + 1)?);
            self.space();
            match self.peek() {
                Some(',') => self.at += 1,
                Some(']') => {}
                _ => return Err(format!("expected ',' or ']' at {}", self.at)),
            }
        }
    }
}

#[cfg(test)]
mod tests;
