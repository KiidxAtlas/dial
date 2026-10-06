//! Markdown -> styled terminal rows (pulldown-cmark). Pure: the same text, width and theme always give
//! the same rows, and partial / unterminated markdown (a model that is still streaming) renders
//! sensibly. Layout is done here, not by the terminal: every row is at most `width` cells, prose is
//! wrapped on word boundaries with its styles intact, code and tables are wrapped hard.
use super::{
    Rows, text,
    theme::{Glyphs, Sym, Theme, Tok},
};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;

/// Render `text` as markdown into rows no wider than `width`. `ascii` forces the ASCII glyph set
/// (box drawing, bullets) regardless of the theme's own preset. All text is sanitized first.
pub fn render(text: &str, width: u16, theme: &Theme, ascii: bool) -> Rows {
    if width == 0 {
        return Vec::new();
    }
    let forced;
    let theme = if ascii && !theme.ascii() {
        forced = theme.clone().with_glyphs(Glyphs::Ascii);
        &forced
    } else {
        theme
    };
    let clean = text::sanitize(text);
    let mut renderer = Renderer::new(theme, width as usize);
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(&clean, options) {
        renderer.event(event);
    }
    renderer.finish()
}

/// Style of one line of a unified diff / patch (`+` added, `-` removed, `@@` hunk, headers, context).
pub fn diff_style(theme: &Theme, line: &str) -> Style {
    if line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("diff ")
        || line.starts_with("index ")
    {
        theme.fg(Tok::DiffContext).add_modifier(Modifier::BOLD)
    } else if line.starts_with('+') {
        theme.fg(Tok::DiffAdded)
    } else if line.starts_with('-') {
        theme.fg(Tok::DiffRemoved)
    } else if line.starts_with("@@") {
        theme.fg(Tok::Accent)
    } else {
        theme.fg(Tok::DiffContext)
    }
}

/// Compact, allocation-light lexer. Unknown fence languages deliberately stay plain.
fn highlight(
    theme: &Theme,
    lang: &str,
    line: &str,
    block_comment: &mut bool,
) -> Vec<Span<'static>> {
    let hash = matches!(
        lang,
        "python" | "py" | "toml" | "yaml" | "yml" | "sh" | "bash" | "shell"
    );
    let c_like = matches!(
        lang,
        "rust"
            | "rs"
            | "javascript"
            | "js"
            | "typescript"
            | "ts"
            | "tsx"
            | "jsx"
            | "go"
            | "c"
            | "cpp"
            | "c++"
            | "h"
            | "java"
    );
    if !hash && !c_like && lang != "json" {
        return vec![Span::styled(line.to_owned(), theme.fg(Tok::MdCodeBlock))];
    }
    let mut spans = Vec::new();
    let mut i = 0;
    while i < line.len() {
        let start = i;
        let rest = &line[i..];
        let ch = rest.chars().next().unwrap();
        let token = if *block_comment || (c_like && rest.starts_with("/*")) {
            let skip = if *block_comment { 0 } else { 2 };
            if let Some(end) = rest[skip..].find("*/") {
                i += skip + end + 2;
                *block_comment = false;
            } else {
                i = line.len();
                *block_comment = true;
            }
            Tok::SyntaxComment
        } else if (hash && ch == '#') || (c_like && rest.starts_with("//")) {
            i = line.len();
            Tok::SyntaxComment
        } else if matches!(ch, '"' | '\'' | '`') {
            i += ch.len_utf8();
            let mut escape = false;
            for c in line[i..].chars() {
                i += c.len_utf8();
                if escape {
                    escape = false;
                } else if c == '\\' {
                    escape = true;
                } else if c == ch {
                    break;
                }
            }
            Tok::SyntaxString
        } else if ch.is_ascii_digit() {
            i += rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_'))
                .map(char::len_utf8)
                .sum::<usize>();
            Tok::SyntaxNumber
        } else if ch.is_alphabetic() || ch == '_' {
            i += rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .map(char::len_utf8)
                .sum::<usize>();
            let word = &line[start..i];
            if matches!(
                word,
                "fn" | "let"
                    | "mut"
                    | "pub"
                    | "impl"
                    | "struct"
                    | "enum"
                    | "trait"
                    | "use"
                    | "mod"
                    | "match"
                    | "if"
                    | "else"
                    | "for"
                    | "while"
                    | "loop"
                    | "in"
                    | "return"
                    | "break"
                    | "continue"
                    | "async"
                    | "await"
                    | "const"
                    | "static"
                    | "true"
                    | "false"
                    | "null"
                    | "None"
                    | "True"
                    | "False"
                    | "def"
                    | "class"
                    | "import"
                    | "from"
                    | "as"
                    | "with"
                    | "try"
                    | "except"
                    | "raise"
                    | "pass"
                    | "yield"
                    | "function"
                    | "var"
                    | "new"
                    | "export"
                    | "default"
                    | "interface"
                    | "type"
                    | "package"
                    | "func"
                    | "defer"
                    | "range"
                    | "switch"
                    | "case"
                    | "select"
                    | "go"
                    | "chan"
                    | "void"
                    | "int"
                    | "char"
                    | "double"
                    | "float"
                    | "bool"
                    | "auto"
                    | "namespace"
                    | "include"
                    | "done"
                    | "then"
                    | "fi"
                    | "do"
            ) {
                Tok::SyntaxKeyword
            } else if ch.is_uppercase() {
                Tok::SyntaxType
            } else {
                Tok::MdCodeBlock
            }
        } else {
            i += ch.len_utf8();
            Tok::MdCodeBlock
        };
        push_merged(&mut spans, &line[start..i], theme.fg(token));
    }
    spans
}

/// Clip styled spans to `max` cells. When something is cut the last cell(s) become `ellipsis`
/// (pass `""` for a plain clip). Never splits a grapheme.
pub fn truncate_spans(spans: Vec<Span<'static>>, max: usize, ellipsis: &str) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| text::width(&s.content)).sum();
    if total <= max {
        return spans;
    }
    let mark_w = text::width(ellipsis);
    let (room, mark) = if mark_w <= max {
        (max - mark_w, ellipsis)
    } else {
        (max, "")
    };
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let mut last_style = Style::default();
    'outer: for span in spans {
        let mut kept = String::new();
        for g in span.content.graphemes(true) {
            let w = text::width(g);
            if used + w > room {
                if !kept.is_empty() {
                    out.push(Span::styled(kept, span.style));
                    last_style = span.style;
                }
                break 'outer;
            }
            kept.push_str(g);
            used += w;
        }
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
        last_style = span.style;
    }
    if !mark.is_empty() {
        out.push(Span::styled(mark.to_owned(), last_style));
    }
    out
}

fn clip_line(line: Line<'static>, max: usize) -> Line<'static> {
    if text::line_width(&line) <= max {
        return line;
    }
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: truncate_spans(line.spans, max, ""),
    }
}

fn is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.is_empty())
}

// ---------------------------------------------------------------------------------------------
// word wrapping of styled inline content

struct Wrapper {
    max: usize,
    out: Rows,
    row: Vec<Span<'static>>,
    used: usize,
    word: Vec<Span<'static>>,
    word_w: usize,
    gap: usize,
    gap_style: Style,
}

fn push_merged(spans: &mut Vec<Span<'static>>, content: &str, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(content);
        return;
    }
    spans.push(Span::styled(content.to_owned(), style));
}

impl Wrapper {
    fn break_line(&mut self) {
        self.out.push(Line::from(std::mem::take(&mut self.row)));
        self.used = 0;
    }

    fn end_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let word = std::mem::take(&mut self.word);
        let w = std::mem::take(&mut self.word_w);
        let gap = if self.used > 0 { self.gap } else { 0 };
        self.gap = 0;
        if w > self.max {
            if self.used > 0 {
                self.break_line();
            }
            for span in word {
                for g in span.content.graphemes(true) {
                    let (g, gw) = if text::width(g) > self.max {
                        ("?", 1)
                    } else {
                        (g, text::width(g))
                    };
                    if self.used + gw > self.max {
                        self.break_line();
                    }
                    push_merged(&mut self.row, g, span.style);
                    self.used += gw;
                }
            }
            return;
        }
        if self.used > 0 {
            if self.used + gap + w > self.max {
                self.break_line();
            } else {
                push_merged(&mut self.row, &" ".repeat(gap), self.gap_style);
                self.used += gap;
            }
        }
        for span in word {
            push_merged(&mut self.row, &span.content, span.style);
        }
        self.used += w;
    }

    fn feed(&mut self, spans: &[Span<'static>]) {
        for span in spans {
            for g in span.content.graphemes(true) {
                if g == "\n" {
                    self.end_word();
                    self.gap = 0;
                    self.break_line();
                } else if g.chars().all(char::is_whitespace) {
                    self.end_word();
                    self.gap += 1;
                    self.gap_style = span.style;
                } else {
                    push_merged(&mut self.word, g, span.style);
                    self.word_w += text::width(g);
                }
            }
        }
    }

    fn finish(mut self) -> Rows {
        self.end_word();
        if !self.row.is_empty() {
            self.break_line();
        }
        self.out
    }
}

/// Word-aware wrap of styled inline content. `\n` spans are hard breaks.
fn wrap_spans(spans: &[Span<'static>], max: usize) -> Rows {
    let mut wrapper = Wrapper {
        max: max.max(1),
        out: Vec::new(),
        row: Vec::new(),
        used: 0,
        word: Vec::new(),
        word_w: 0,
        gap: 0,
        gap_style: Style::default(),
    };
    wrapper.feed(spans);
    wrapper.finish()
}

// ---------------------------------------------------------------------------------------------
// block structure

enum FrameKind {
    Root,
    Quote,
    List {
        next: Option<u64>,
        items: usize,
        prev_loose: bool,
    },
    Item,
}

struct Frame {
    kind: FrameKind,
    rows: Rows,
    /// Blocks inside are separated by a blank row (tight list items are not).
    loose: bool,
    blocks: usize,
    marker: Vec<Span<'static>>,
    /// Cells taken by the quote bar / list marker in front of this frame's content.
    indent: usize,
}

impl Frame {
    fn new(kind: FrameKind, loose: bool) -> Self {
        Frame {
            kind,
            rows: Vec::new(),
            loose,
            blocks: 0,
            marker: Vec::new(),
            indent: 0,
        }
    }
}

type Cell = Vec<Span<'static>>;

struct TableBuf {
    aligns: Vec<Alignment>,
    head: Vec<Cell>,
    rows: Vec<Vec<Cell>>,
    cur: Vec<Cell>,
}

struct Renderer<'a> {
    theme: &'a Theme,
    width: usize,
    frames: Vec<Frame>,
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    /// Open links / images: destination and where their text starts in `inline`.
    links: Vec<(String, usize)>,
    /// Open code block: info-string language and accumulated text.
    code: Option<(String, String)>,
    table: Option<TableBuf>,
}

const MIN_CODE_BOX: usize = 8;
const MIN_COLUMN: usize = 3;

impl<'a> Renderer<'a> {
    fn new(theme: &'a Theme, width: usize) -> Self {
        Renderer {
            theme,
            width,
            frames: vec![Frame::new(FrameKind::Root, true)],
            inline: Vec::new(),
            styles: Vec::new(),
            links: Vec::new(),
            code: None,
            table: None,
        }
    }

    fn avail(&self) -> usize {
        let indent: usize = self.frames.iter().map(|f| f.indent).sum();
        self.width.saturating_sub(indent).max(1)
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::default(), |acc, s| acc.patch(*s))
    }

    fn top(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("root frame is never popped")
    }

    fn add_span(&mut self, content: &str, style: Style) {
        if content.is_empty() {
            return;
        }
        push_merged(&mut self.inline, content, style);
    }

    fn add_text(&mut self, content: &str) {
        let style = self.style();
        self.add_span(content, style);
    }

    fn begin_block(&mut self) {
        let frame = self.top();
        if frame.blocks > 0 && frame.loose {
            frame.rows.push(Line::default());
        }
        frame.blocks += 1;
    }

    fn flush_inline(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.inline);
        let rows = wrap_spans(&spans, self.avail());
        if rows.is_empty() {
            return;
        }
        self.begin_block();
        self.top().rows.extend(rows);
    }

    fn rule_row(&self, cells: usize, tok: Tok) -> Line<'static> {
        Line::from(Span::styled(
            self.theme.sym(Sym::Hr).repeat(cells),
            self.theme.fg(tok),
        ))
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => match &mut self.code {
                Some((_, buf)) => buf.push_str(&t),
                None => self.add_text(&t),
            },
            Event::Code(t) => {
                let style = self.style().patch(self.theme.fg(Tok::MdCode));
                self.add_span(&t, style);
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => self.add_text(&t),
            Event::Html(t) | Event::InlineHtml(t) => {
                let style = self.style().patch(self.theme.fg(Tok::Muted));
                self.add_span(t.trim_end_matches('\n'), style);
                if t.ends_with('\n') {
                    self.add_span("\n", style);
                }
            }
            Event::FootnoteReference(label) => {
                let style = self.style().patch(self.theme.fg(Tok::Muted));
                self.add_span(&format!("[^{label}]"), style);
            }
            Event::SoftBreak => self.add_text(" "),
            Event::HardBreak => self.add_text("\n"),
            Event::Rule => {
                self.flush_inline();
                self.begin_block();
                let row = self.rule_row(self.avail(), Tok::MdHr);
                self.top().rows.push(row);
            }
            Event::TaskListMarker(done) => {
                let (mark, style) = if done {
                    ("[x] ", self.theme.fg(Tok::Success))
                } else {
                    ("[ ] ", self.theme.fg(Tok::Muted))
                };
                if let Some(frame) = self
                    .frames
                    .iter_mut()
                    .rev()
                    .find(|f| matches!(f.kind, FrameKind::Item))
                {
                    frame.marker = vec![Span::styled(mark, style)];
                    frame.indent = text::width(mark);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush_inline();
                if matches!(self.top().kind, FrameKind::Item) {
                    self.top().loose = true;
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_inline();
                self.styles.push(self.theme.fg(Tok::MdHeading));
                let n = heading_number(level);
                if n >= 3 {
                    self.add_text(&format!("{} ", "#".repeat(n)));
                }
            }
            Tag::BlockQuote(_) => {
                self.flush_inline();
                self.begin_block();
                let mut frame = Frame::new(FrameKind::Quote, true);
                frame.indent = text::width(self.theme.sym(Sym::Quote)) + 1;
                self.frames.push(frame);
                self.styles.push(self.theme.fg(Tok::MdQuote));
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
                        .next()
                        .unwrap_or("")
                        .to_owned(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::HtmlBlock => self.flush_inline(),
            Tag::List(start) => {
                self.flush_inline();
                self.begin_block();
                self.frames.push(Frame::new(
                    FrameKind::List {
                        next: start,
                        items: 0,
                        prev_loose: false,
                    },
                    false,
                ));
            }
            Tag::Item => {
                self.flush_inline();
                let mut blank = false;
                let mut marker = String::new();
                if let FrameKind::List {
                    next,
                    items,
                    prev_loose,
                } = &mut self.top().kind
                {
                    blank = *items > 0 && *prev_loose;
                    marker = match next {
                        Some(n) => {
                            let m = format!("{n}. ");
                            *n += 1;
                            m
                        }
                        None => String::new(),
                    };
                    *items += 1;
                }
                if blank {
                    self.top().rows.push(Line::default());
                }
                if marker.is_empty() {
                    marker = format!("{} ", self.theme.sym(Sym::Bullet));
                }
                let mut frame = Frame::new(FrameKind::Item, false);
                frame.indent = text::width(&marker);
                frame.marker = vec![Span::styled(marker, self.theme.fg(Tok::MdBullet))];
                self.frames.push(frame);
            }
            Tag::FootnoteDefinition(label) => {
                self.flush_inline();
                let style = self.theme.fg(Tok::Muted);
                self.add_span(&format!("[^{label}]: "), style);
            }
            Tag::Table(aligns) => {
                self.flush_inline();
                self.table = Some(TableBuf {
                    aligns,
                    head: Vec::new(),
                    rows: Vec::new(),
                    cur: Vec::new(),
                });
            }
            Tag::TableHead => self
                .styles
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.cur.clear();
                }
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self
                .styles
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .styles
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.styles.push(self.theme.fg(Tok::MdLink));
                self.links.push((dest_url.into_string(), self.inline.len()));
            }
            Tag::Image { dest_url, .. } => {
                self.styles.push(self.theme.fg(Tok::MdLink));
                self.add_text("[image: ");
                self.links.push((dest_url.into_string(), self.inline.len()));
            }
            Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock | TagEnd::FootnoteDefinition => {
                self.flush_inline()
            }
            TagEnd::Heading(level) => {
                self.flush_inline();
                self.styles.pop();
                if heading_number(level) == 1 {
                    let row = self.rule_row(self.avail(), Tok::MdHr);
                    self.top().rows.push(row);
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.styles.pop();
                if let Some(frame) = self.pop_frame() {
                    let rows = self.quote_rows(frame.rows);
                    self.top().rows.extend(rows);
                }
            }
            TagEnd::CodeBlock => {
                if let Some((lang, body)) = self.code.take() {
                    self.code_block(&lang, &body);
                }
            }
            TagEnd::List(_) => {
                self.flush_inline();
                if let Some(frame) = self.pop_frame() {
                    self.top().rows.extend(frame.rows);
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                if let Some(frame) = self.pop_frame() {
                    let loose = frame.loose;
                    let rows = item_rows(frame.rows, frame.marker, frame.indent);
                    let parent = self.top();
                    if let FrameKind::List { prev_loose, .. } = &mut parent.kind {
                        *prev_loose = loose;
                    }
                    parent.rows.extend(rows);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    let rows = self.table_rows(&table);
                    if !rows.is_empty() {
                        self.begin_block();
                        self.top().rows.extend(rows);
                    }
                }
            }
            TagEnd::TableHead => {
                self.styles.pop();
                if let Some(t) = &mut self.table {
                    t.head = std::mem::take(&mut t.cur);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.cur);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline);
                if let Some(t) = &mut self.table {
                    t.cur.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                self.styles.pop();
                let is_image = matches!(tag, TagEnd::Image);
                if is_image {
                    self.add_text("]");
                }
                if let Some((url, start)) = self.links.pop() {
                    let label: String = self.inline[start.min(self.inline.len())..]
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect();
                    let label = label.trim_end_matches(']');
                    if !url.is_empty()
                        && label != url
                        && label.trim_start_matches("mailto:") != url.trim_start_matches("mailto:")
                    {
                        let style = self.style().patch(self.theme.fg(Tok::MdLinkUrl));
                        self.add_span(&format!(" ({url})"), style);
                    }
                }
            }
            TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn pop_frame(&mut self) -> Option<Frame> {
        if self.frames.len() > 1 {
            self.frames.pop()
        } else {
            None
        }
    }

    fn quote_rows(&self, rows: Rows) -> Rows {
        let bar = Span::styled(
            format!("{} ", self.theme.sym(Sym::Quote)),
            self.theme.fg(Tok::MdQuoteBorder),
        );
        if rows.is_empty() {
            return vec![Line::from(bar)];
        }
        rows.into_iter()
            .map(|mut line| {
                line.spans.insert(0, bar.clone());
                line
            })
            .collect()
    }

    fn finish(mut self) -> Rows {
        self.flush_inline();
        if let Some((lang, body)) = self.code.take() {
            self.code_block(&lang, &body);
        }
        while self.frames.len() > 1 {
            let frame = self.frames.pop().expect("checked length");
            let rows = match frame.kind {
                FrameKind::Quote => self.quote_rows(frame.rows),
                FrameKind::Item => item_rows(frame.rows, frame.marker, frame.indent),
                _ => frame.rows,
            };
            self.top().rows.extend(rows);
        }
        let width = self.width;
        let root = self.frames.pop().expect("root frame");
        root.rows
            .into_iter()
            .map(|line| clip_line(line, width))
            .collect()
    }

    // ---- code ----------------------------------------------------------------------------

    fn code_block(&mut self, lang: &str, body: &str) {
        let avail = self.avail();
        let theme = self.theme;
        let diff = is_diff(lang, body);
        let body = body.strip_suffix('\n').unwrap_or(body);
        let mut block_comment = false;
        let language = lang
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let lines: Rows = if body.is_empty() {
            Vec::new()
        } else {
            body.split('\n')
                .map(|l| {
                    Line::from(if diff {
                        vec![Span::styled(l.to_owned(), diff_style(theme, l))]
                    } else {
                        highlight(theme, &language, l, &mut block_comment)
                    })
                })
                .collect()
        };
        let mut rows: Rows = Vec::new();
        if avail < MIN_CODE_BOX {
            for line in lines {
                rows.extend(text::wrap(vec![line], avail));
            }
        } else {
            let border = theme.fg(Tok::MdCodeBorder);
            let inner = avail - 4;
            let h = theme.sym(Sym::BoxH);
            let mut top = vec![Span::styled(
                format!("{}{h}", theme.sym(Sym::BoxTl)),
                border,
            )];
            let label = text::ellipsize(lang, avail.saturating_sub(6), theme.ascii());
            if label.is_empty() {
                top.push(Span::styled(
                    format!("{}{}", h.repeat(avail - 3), theme.sym(Sym::BoxTr)),
                    border,
                ));
            } else {
                let fill = avail.saturating_sub(5 + text::width(&label));
                top.push(Span::styled(" ", border));
                top.push(Span::styled(label, theme.fg(Tok::Muted)));
                top.push(Span::styled(
                    format!(" {}{}", h.repeat(fill), theme.sym(Sym::BoxTr)),
                    border,
                ));
            }
            rows.push(Line::from(top));
            let v = theme.sym(Sym::BoxV);
            for line in lines {
                for wrapped in text::wrap(vec![line], inner) {
                    let used = text::line_width(&wrapped);
                    let mut spans = vec![Span::styled(format!("{v} "), border)];
                    spans.extend(wrapped.spans);
                    spans.push(Span::styled(
                        format!("{} {v}", " ".repeat(inner.saturating_sub(used))),
                        border,
                    ));
                    rows.push(Line::from(spans));
                }
            }
            rows.push(Line::from(Span::styled(
                format!(
                    "{}{}{}",
                    theme.sym(Sym::BoxBl),
                    h.repeat(avail - 2),
                    theme.sym(Sym::BoxBr)
                ),
                border,
            )));
        }
        self.begin_block();
        self.top().rows.extend(rows);
    }

    // ---- tables --------------------------------------------------------------------------

    fn table_rows(&self, table: &TableBuf) -> Rows {
        let columns = table
            .aligns
            .len()
            .max(table.head.len())
            .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
        if columns == 0 {
            return Vec::new();
        }
        let avail = self.avail();
        let empty: Cell = Vec::new();
        let cell = |row: &Vec<Cell>, i: usize| -> Cell {
            row.get(i).cloned().unwrap_or_else(|| empty.clone())
        };
        let head: Vec<Cell> = (0..columns).map(|i| cell(&table.head, i)).collect();
        let body: Vec<Vec<Cell>> = table
            .rows
            .iter()
            .map(|r| (0..columns).map(|i| cell(r, i)).collect())
            .collect();

        let natural: Vec<usize> = (0..columns)
            .map(|i| {
                std::iter::once(&head[i])
                    .chain(body.iter().map(|r| &r[i]))
                    .map(|c| cell_width(c))
                    .max()
                    .unwrap_or(1)
                    .max(1)
            })
            .collect();
        let overhead = 3 * columns + 1;
        let room = avail.saturating_sub(overhead);
        let floor: usize = natural.iter().map(|&w| w.min(MIN_COLUMN)).sum();
        if avail <= overhead || floor > room {
            return self.table_records(&head, &body, avail);
        }
        let widths = fit_columns(&natural, room);
        let border = self.theme.fg(Tok::MdCodeBorder);
        let theme = self.theme;
        let h = theme.sym(Sym::BoxH);
        let rule = |left: Sym, mid: Sym, right: Sym| -> Line<'static> {
            let mut s = theme.sym(left).to_owned();
            for (i, w) in widths.iter().enumerate() {
                if i > 0 {
                    s.push_str(theme.sym(mid));
                }
                s.push_str(&h.repeat(w + 2));
            }
            s.push_str(theme.sym(right));
            Line::from(Span::styled(s, border))
        };
        let mut rows = vec![rule(Sym::BoxTl, Sym::TeeDown, Sym::BoxTr)];
        rows.extend(grid_row(theme, &head, &widths, &table.aligns));
        if !body.is_empty() {
            rows.push(rule(Sym::TeeL, Sym::Cross, Sym::TeeR));
        }
        for row in &body {
            rows.extend(grid_row(theme, row, &widths, &table.aligns));
        }
        rows.push(rule(Sym::BoxBl, Sym::TeeUp, Sym::BoxBr));
        rows
    }

    /// Too narrow for a grid: one `header: value` row group per record.
    fn table_records(&self, head: &[Cell], body: &[Vec<Cell>], avail: usize) -> Rows {
        let mut rows = Vec::new();
        let bold = Style::default().add_modifier(Modifier::BOLD);
        for (n, record) in body.iter().enumerate() {
            if n > 0 {
                rows.push(self.rule_row(avail, Tok::MdHr));
            }
            for (i, value) in record.iter().enumerate() {
                let mut spans: Vec<Span<'static>> = head[i].clone();
                if spans.is_empty() {
                    spans.push(Span::styled(format!("{}", i + 1), bold));
                }
                spans.push(Span::styled(": ", bold));
                spans.extend(value.iter().cloned());
                rows.extend(wrap_spans(&spans, avail));
            }
        }
        if body.is_empty() {
            let names: Vec<Span<'static>> = head
                .iter()
                .enumerate()
                .flat_map(|(i, c)| {
                    let mut v = if i > 0 {
                        vec![Span::raw(", ")]
                    } else {
                        Vec::new()
                    };
                    v.extend(c.iter().cloned());
                    v
                })
                .collect();
            rows.extend(wrap_spans(&names, avail));
        }
        rows
    }
}

fn heading_number(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// A list item's rows: marker on the first row, hanging indent on the rest (blank rows stay blank).
fn item_rows(rows: Rows, marker: Vec<Span<'static>>, indent: usize) -> Rows {
    if rows.is_empty() {
        return vec![Line::from(marker)];
    }
    let pad = " ".repeat(indent);
    let mut marker = Some(marker);
    rows.into_iter()
        .map(|mut line| {
            match marker.take() {
                Some(m) => {
                    let mut spans = m;
                    spans.extend(line.spans);
                    line.spans = spans;
                }
                None if !is_blank(&line) => line.spans.insert(0, Span::raw(pad.clone())),
                None => {}
            }
            line
        })
        .collect()
}

fn is_diff(lang: &str, body: &str) -> bool {
    if ["diff", "patch", "udiff"]
        .iter()
        .any(|l| lang.eq_ignore_ascii_case(l))
    {
        return true;
    }
    lang.is_empty()
        && body
            .lines()
            .any(|l| l.starts_with("@@ ") || l.starts_with("diff --git"))
}

fn cell_width(cell: &[Span<'static>]) -> usize {
    let flat: String = cell.iter().map(|s| s.content.as_ref()).collect();
    flat.split('\n').map(text::width).max().unwrap_or(0)
}

/// Shrink the widest columns first (water-filling) so the sum fits `room`; spare cells go to the
/// leftmost shrunk columns. Callers guarantee `sum(min(natural, MIN_COLUMN)) <= room`.
fn fit_columns(natural: &[usize], room: usize) -> Vec<usize> {
    if natural.iter().sum::<usize>() <= room {
        return natural.to_vec();
    }
    let (mut lo, mut hi) = (
        MIN_COLUMN,
        natural.iter().copied().max().unwrap_or(MIN_COLUMN),
    );
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if natural.iter().map(|&w| w.min(mid)).sum::<usize>() <= room {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut widths: Vec<usize> = natural.iter().map(|&w| w.min(lo)).collect();
    let mut spare = room - widths.iter().sum::<usize>();
    for (w, &n) in widths.iter_mut().zip(natural) {
        if spare == 0 {
            break;
        }
        if *w < n {
            *w += 1;
            spare -= 1;
        }
    }
    widths
}

/// One table row as one or more physical rows (cells wrap inside their column).
fn grid_row(theme: &Theme, cells: &[Cell], widths: &[usize], aligns: &[Alignment]) -> Rows {
    let border = theme.fg(Tok::MdCodeBorder);
    let v = theme.sym(Sym::BoxV);
    let wrapped: Vec<Rows> = cells
        .iter()
        .zip(widths)
        .map(|(c, &w)| wrap_spans(c, w))
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
    (0..height)
        .map(|y| {
            let mut spans = vec![Span::styled(v.to_owned(), border)];
            for (i, &w) in widths.iter().enumerate() {
                let line = wrapped.get(i).and_then(|l| l.get(y));
                let used = line.map_or(0, |l| text::line_width(l));
                let extra = w.saturating_sub(used);
                let (left, right) = match aligns.get(i).copied().unwrap_or(Alignment::None) {
                    Alignment::Right => (extra, 0),
                    Alignment::Center => (extra / 2, extra - extra / 2),
                    Alignment::Left | Alignment::None => (0, extra),
                };
                spans.push(Span::raw(" ".repeat(left + 1)));
                if let Some(line) = line {
                    spans.extend(line.spans.iter().cloned());
                }
                spans.push(Span::raw(" ".repeat(right + 1)));
                spans.push(Span::styled(v.to_owned(), border));
            }
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_tokens_preserve_text_and_degrade() {
        let source = "let value = \"hello\"; // comment";
        let colored = highlight(&theme(), "rust", source, &mut false);
        assert_eq!(
            colored
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            source
        );
        assert_eq!(colored[0].style, theme().fg(Tok::SyntaxKeyword));
        assert!(
            colored
                .iter()
                .any(|s| s.content == "\"hello\"" && s.style == theme().fg(Tok::SyntaxString))
        );
        assert!(
            colored
                .iter()
                .any(|s| s.content == "// comment" && s.style == theme().fg(Tok::SyntaxComment))
        );
        for mode in [
            super::super::theme::ColorMode::None,
            super::super::theme::ColorMode::Ansi16,
            super::super::theme::ColorMode::Ansi256,
        ] {
            let palette = theme().downgraded(mode);
            let spans = highlight(&palette, "rust", source, &mut false);
            assert!(
                spans
                    .iter()
                    .all(|s| !matches!(s.style.fg, Some(ratatui::style::Color::Rgb(..))))
            );
            if mode == super::super::theme::ColorMode::None {
                assert!(spans.iter().all(|s| s.style.fg.is_none()));
            }
        }
        let unknown = highlight(&theme(), "unknown", source, &mut false);
        assert_eq!(unknown.len(), 1);
        let mut comment = false;
        highlight(&theme(), "c", "/* open", &mut comment);
        assert!(comment);
        let closed = highlight(&theme(), "c", "end */ int n = 42;", &mut comment);
        assert!(!comment);
        assert!(
            closed
                .iter()
                .any(|s| s.content == "42" && s.style == theme().fg(Tok::SyntaxNumber))
        );
    }

    fn theme() -> Theme {
        Theme::dark()
    }

    fn plain(rows: &Rows) -> Vec<String> {
        rows.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn md(src: &str, width: u16) -> Rows {
        render(src, width, &theme(), false)
    }

    fn assert_fits(rows: &Rows, width: u16) {
        for (i, row) in rows.iter().enumerate() {
            let w = text::line_width(row);
            assert!(
                w <= width as usize,
                "row {i} is {w} cells wide, limit {width}: {:?}",
                plain(&vec![row.clone()])
            );
        }
    }

    /// The span containing `needle`, so a test can assert the style a user would see on that text.
    fn style_of(rows: &Rows, needle: &str) -> Style {
        rows.iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(needle))
            .unwrap_or_else(|| panic!("no span with {needle:?} in {:?}", plain(rows)))
            .style
    }

    const SAMPLE: &str = "# Title\n\nSome *emphasis*, **strong**, ~~gone~~ and `code` with a [link](https://example.com/a).\n\n- one\n- two\n  - nested\n\n1. first\n2. second\n\n> quote\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n";

    #[test]
    fn empty_and_zero_width_produce_nothing() {
        assert!(md("", 40).is_empty());
        assert!(md("   \n\n  ", 40).is_empty());
        assert!(md("hello", 0).is_empty());
    }

    #[test]
    fn every_construct_fits_every_width() {
        for width in 1..=100u16 {
            assert_fits(&md(SAMPLE, width), width);
        }
    }

    #[test]
    fn prose_wraps_on_words_without_losing_text() {
        let rows = md("alpha beta gamma delta epsilon zeta eta theta", 14);
        assert_fits(&rows, 14);
        assert_eq!(
            plain(&rows),
            ["alpha beta", "gamma delta", "epsilon zeta", "eta theta"]
        );
    }

    #[test]
    fn paragraphs_are_separated_by_one_blank_row() {
        assert_eq!(
            plain(&md("one\n\ntwo\n\n\n\nthree", 20)),
            ["one", "", "two", "", "three"]
        );
    }

    #[test]
    fn soft_break_is_a_space_and_hard_break_a_new_row() {
        assert_eq!(plain(&md("one\ntwo", 20)), ["one two"]);
        assert_eq!(plain(&md("one  \ntwo", 20)), ["one", "two"]);
        assert_eq!(plain(&md("one\\\ntwo", 20)), ["one", "two"]);
    }

    #[test]
    fn emphasis_strong_strike_and_code_carry_their_styles() {
        let rows = md("plain *slanted* **heavy** ~~gone~~ `mono`", 80);
        let t = theme();
        assert!(
            style_of(&rows, "slanted")
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert!(
            style_of(&rows, "heavy")
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            style_of(&rows, "gone")
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert_eq!(style_of(&rows, "mono").fg, t.fg(Tok::MdCode).fg);
        let p = style_of(&rows, "plain");
        assert!(p.add_modifier.is_empty());
    }

    #[test]
    fn nested_emphasis_combines_and_survives_wrapping() {
        let rows = md("**bold and _both_ here** tail", 9);
        assert_fits(&rows, 9);
        let both: Vec<_> = rows
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|s| s.content.contains("both"))
            .collect();
        assert!(both.iter().all(|s| {
            s.style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        }));
        for word in ["bold", "and", "here"] {
            assert!(
                style_of(&rows, word).add_modifier.contains(Modifier::BOLD),
                "{word}"
            );
        }
        assert!(
            !style_of(&rows, "tail")
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn headings_are_styled_and_h1_gets_a_rule() {
        let t = theme();
        let rows = md("# Big\n\n## Medium\n\n### Small", 20);
        let text = plain(&rows);
        assert_eq!(text[0], "Big");
        assert!(
            text[1].chars().all(|c| t.sym(Sym::Hr).contains(c)) && text[1].chars().count() == 20
        );
        assert!(text.contains(&"Medium".to_owned()));
        assert!(text.contains(&"### Small".to_owned()));
        assert_eq!(style_of(&rows, "Big").fg, t.fg(Tok::MdHeading).fg);
        assert!(
            style_of(&rows, "Medium")
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn links_show_dim_url_unless_it_is_the_text() {
        let t = theme();
        let rows = md("see [docs](https://x.dev/a) now", 80);
        assert_eq!(plain(&rows), ["see docs (https://x.dev/a) now"]);
        assert_eq!(style_of(&rows, "docs").fg, t.fg(Tok::MdLink).fg);
        assert_eq!(
            style_of(&rows, "https://x.dev/a").fg,
            t.fg(Tok::MdLinkUrl).fg
        );
        let auto = md("<https://x.dev/a>", 80);
        assert_eq!(plain(&auto), ["https://x.dev/a"]);
    }

    #[test]
    fn fenced_code_is_boxed_with_language_label() {
        let rows = md("```rust\nlet x = 1;\n```", 20);
        let text = plain(&rows);
        assert_eq!(text.len(), 3);
        assert!(text[0].starts_with('╭') && text[0].ends_with('╮') && text[0].contains(" rust "));
        assert_eq!(text[1], "│ let x = 1;       │");
        assert!(text[2].starts_with('╰') && text[2].ends_with('╯'));
        assert!(text.iter().all(|l| text::width(l) == 20));
    }

    #[test]
    fn code_lines_wrap_inside_the_box_and_indented_code_is_boxed() {
        let rows = md("    aaaaaaaaaaaaaaaaaaaa", 12);
        assert_fits(&rows, 12);
        let text = plain(&rows);
        assert!(text.len() >= 4, "{text:?}");
        assert!(text.iter().all(|l| text::width(l) == 12));
        assert!(text.first().unwrap().starts_with('╭'));
    }

    #[test]
    fn narrow_width_drops_the_box_but_keeps_the_code() {
        let rows = md("```\nabcdefghij\n```", 5);
        assert_fits(&rows, 5);
        assert_eq!(plain(&rows), ["abcde", "fghij"]);
    }

    #[test]
    fn diff_fences_color_added_removed_and_context() {
        let t = theme();
        let rows = md("```diff\n@@ -1 +1 @@\n-old\n+new\n same\n```", 30);
        assert_eq!(style_of(&rows, "+new").fg, t.fg(Tok::DiffAdded).fg);
        assert_eq!(style_of(&rows, "-old").fg, t.fg(Tok::DiffRemoved).fg);
        assert_eq!(style_of(&rows, " same").fg, t.fg(Tok::DiffContext).fg);
        assert_eq!(style_of(&rows, "@@ -1").fg, t.fg(Tok::Accent).fg);
        // the same lines in a normal language fence are not colored as a diff
        let plain_rows = md("```rust\n-old\n+new\n```", 30);
        assert!(plain_rows.iter().flat_map(|line| &line.spans).all(|span| {
            span.style.fg != t.fg(Tok::DiffAdded).fg && span.style.fg != t.fg(Tok::DiffRemoved).fg
        }));
    }

    #[test]
    fn unlabeled_fence_that_looks_like_a_patch_is_colored() {
        let rows = md("```\ndiff --git a/x b/x\n@@ -1 +1 @@\n+added\n```", 40);
        assert_eq!(style_of(&rows, "+added").fg, theme().fg(Tok::DiffAdded).fg);
    }

    #[test]
    fn lists_nest_with_hanging_indents() {
        let rows = md(
            "- alpha beta gamma delta\n  - inner item that is long\n- two",
            16,
        );
        assert_fits(&rows, 16);
        let text = plain(&rows);
        let bullet = theme().sym(Sym::Bullet).to_owned();
        assert!(text[0].starts_with(&format!("{bullet} alpha")), "{text:?}");
        assert!(
            text[1].starts_with("  gamma"),
            "continuation hangs under the text: {text:?}"
        );
        let nested = text.iter().position(|l| l.contains("inner")).unwrap();
        assert!(
            text[nested].starts_with(&format!("  {bullet} inner")),
            "{text:?}"
        );
        assert!(text.last().unwrap().starts_with(&format!("{bullet} two")));
    }

    #[test]
    fn ordered_lists_keep_their_numbers() {
        let text = plain(&md("3. three\n4. four\n   more text here", 30));
        assert_eq!(text, ["3. three", "4. four more text here"]);
    }

    #[test]
    fn task_lists_show_checkboxes() {
        let rows = md("- [x] done\n- [ ] todo", 30);
        assert_eq!(plain(&rows), ["[x] done", "[ ] todo"]);
        assert_eq!(style_of(&rows, "[x]").fg, theme().fg(Tok::Success).fg);
    }

    #[test]
    fn loose_lists_get_blank_rows_between_items_and_tight_ones_do_not() {
        assert_eq!(plain(&md("- a\n\n- b", 20)).len(), 3);
        assert_eq!(plain(&md("- a\n- b", 20)).len(), 2);
    }

    #[test]
    fn blockquotes_carry_a_bar_on_every_row_including_nested_blocks() {
        let t = theme();
        let rows = md("> first line\n>\n> second line that wraps around", 16);
        assert_fits(&rows, 16);
        let bar = t.sym(Sym::Quote);
        assert!(
            plain(&rows).iter().all(|l| l.starts_with(bar)),
            "{:?}",
            plain(&rows)
        );
        assert_eq!(rows[0].spans[0].style.fg, t.fg(Tok::MdQuoteBorder).fg);
        assert!(
            style_of(&rows, "first")
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn horizontal_rule_spans_the_width() {
        let rows = md("a\n\n---\n\nb", 10);
        assert_eq!(plain(&rows)[2].chars().count(), 10);
        assert_eq!(rows[2].spans[0].style.fg, theme().fg(Tok::MdHr).fg);
    }

    const TABLE: &str = "| name | qty | note |\n|:-----|----:|:----:|\n| apple | 3 | red |\n| fig | 12 | purple |\n";

    #[test]
    fn table_is_a_box_grid_with_alignment() {
        let text = plain(&md(TABLE, 60));
        assert_eq!(text.len(), 6, "{text:?}");
        assert!(text[0].starts_with('╭') && text[0].contains('┬'));
        assert!(text[2].starts_with('├') && text[2].contains('┼'));
        assert!(text[5].starts_with('╰') && text[5].contains('┴'));
        assert!(text.iter().all(|l| text::width(l) == text::width(&text[0])));
        assert!(
            text[3].contains("│ apple │   3 │"),
            "numbers are right aligned: {text:?}"
        );
        assert!(text[4].contains("│  12 │"), "{text:?}");
    }

    #[test]
    fn table_header_is_bold() {
        assert!(
            style_of(&md(TABLE, 60), "name")
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn table_shrinks_and_wraps_cells_to_fit() {
        let src = "| key | description |\n|---|---|\n| a | a rather long description that must wrap inside its column |\n";
        for width in [20u16, 24, 30, 40] {
            let rows = md(src, width);
            assert_fits(&rows, width);
            let text = plain(&rows);
            assert!(text[0].starts_with('╭'), "width {width}: {text:?}");
            let joined = text.join(" ");
            for word in ["rather", "long", "wrap", "column"] {
                assert!(joined.contains(word), "width {width} lost {word}: {text:?}");
            }
        }
    }

    #[test]
    fn table_too_narrow_falls_back_to_key_value_records() {
        let rows = md(TABLE, 12);
        assert_fits(&rows, 12);
        let text = plain(&rows);
        assert!(text.iter().all(|l| !l.contains('┬')), "{text:?}");
        assert!(text.contains(&"name: apple".to_owned()), "{text:?}");
        assert!(text.contains(&"qty: 12".to_owned()), "{text:?}");
        assert!(
            text.iter().any(|l| l.contains('─')),
            "records are separated by a rule: {text:?}"
        );
    }

    #[test]
    fn unterminated_fence_still_renders_a_closed_box() {
        let rows = md("text\n\n```py\nprint(1)\nprint(", 24);
        assert_fits(&rows, 24);
        let text = plain(&rows);
        assert!(text.iter().any(|l| l.contains("print(1)")));
        assert!(text.last().unwrap().starts_with('╰'));
    }

    #[test]
    fn unterminated_markup_is_shown_literally() {
        assert_eq!(plain(&md("a **bold start", 40)), ["a **bold start"]);
        assert_eq!(plain(&md("a *x", 40)), ["a *x"]);
        assert_eq!(plain(&md("[text](http://x", 40)), ["[text](http://x"]);
        assert!(!plain(&md("| a | b |\n|---", 40)).is_empty());
    }

    #[test]
    fn every_prefix_of_a_document_renders_within_width() {
        // what streaming does: render each growing prefix
        for end in (0..SAMPLE.len()).filter(|i| SAMPLE.is_char_boundary(*i)) {
            for width in [3u16, 7, 20, 60] {
                assert_fits(&md(&SAMPLE[..end], width), width);
            }
        }
    }

    #[test]
    fn huge_unbroken_line_is_hard_wrapped() {
        let rows = md(&"x".repeat(10_000), 37);
        assert_fits(&rows, 37);
        assert_eq!(rows.len(), 10_000usize.div_ceil(37));
        let code = md(&format!("```\n{}\n```", "y".repeat(5_000)), 40);
        assert_fits(&code, 40);
    }

    #[test]
    fn wide_characters_and_emoji_never_overflow() {
        let src = "日本語のテキストは幅が二つです 🎉🎉🎉 mixed ascii 汉字\n\n```\n日本語日本語日本語\n```\n\n| 名前 | 値 |\n|--|--|\n| 日本 | 🎉 |";
        for width in 1..=40u16 {
            assert_fits(&md(src, width), width);
        }
        let rows = md("日本語日本語", 5);
        assert_eq!(plain(&rows), ["日本", "語日", "本語"]);
    }

    #[test]
    fn hostile_escape_sequences_never_reach_the_rows() {
        let src = "safe \x1b[31mred\x1b[0m \x1b]0;pwned\x07title \x1b]8;;http://evil\x1b\\link\x1b]8;;\x1b\\ \u{202e}rtl\r\n\n```\n\x1b[2Jcode\x1b[H\n```\n| \x1b[1mh | b |\n|--|--|\n| \u{9b}31mx | y |";
        for row in md(src, 40) {
            for span in row.spans {
                assert!(
                    !span
                        .content
                        .chars()
                        .any(|c| c.is_control() || c == '\u{202e}'),
                    "{:?}",
                    span.content
                );
                assert!(
                    !span.content.contains("pwned")
                        && !span.content.contains("[31m")
                        && !span.content.contains("[2J")
                );
            }
        }
    }

    #[test]
    fn html_is_shown_as_dim_text_not_dropped() {
        let rows = md("before <b>x</b> after", 40);
        let joined = plain(&rows).join("");
        assert!(
            joined.contains("<b>") && joined.contains("after"),
            "{joined}"
        );
    }

    #[test]
    fn ascii_mode_uses_ascii_glyphs_for_structure() {
        let rows = render(
            "- a\n\n```rs\nx\n```\n\n> q\n\n| a |\n|--|\n| b |",
            30,
            &theme(),
            true,
        );
        for row in plain(&rows) {
            assert!(row.is_ascii(), "{row:?}");
        }
    }
}
