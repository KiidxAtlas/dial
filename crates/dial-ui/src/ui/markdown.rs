//! Markdown -> styled terminal rows (pulldown-cmark). Pure: the same text, width and theme always give
//! the same rows, and partial / unterminated markdown (a model that is still streaming) renders
//! sensibly. Layout is done here, not by the terminal: every row is at most `width` cells, prose is
//! wrapped on word boundaries with its styles intact, code and tables are wrapped hard.
use super::{
    Rows, chrome, text,
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
    render_split(&clean, width, theme).0
}

const OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS);

/// Render already-sanitized markdown (`text::sanitize` output) and report the latest *stable cut*
/// `(offset, rows)`: `clean[..offset]` ends a complete top-level block, renders to exactly the first
/// `rows` rows, and no amount of appended text can change those rows any more. A cut is only taken
/// where the parser itself saw the next top-level block start, on a complete line, after a blank
/// line, so open fences, HTML blocks, lists that may continue and setext underlines never split.
/// Link reference definitions are the one retroactive construct; callers re-render fully when the
/// text has any (see `has_reference_definition`).
pub(crate) fn render_split(
    clean: &str,
    width: u16,
    theme: &Theme,
) -> (Rows, Option<(usize, usize)>) {
    if width == 0 {
        return (Vec::new(), None);
    }
    let mut renderer = Renderer::new(theme, width as usize);
    let mut depth = 0usize;
    // Root rows when the previous top-level block ended.
    let mut ended: Option<usize> = None;
    let mut cut = None;
    for (event, range) in Parser::new_ext(clean, OPTIONS).into_offset_iter() {
        let opens = matches!(event, Event::Start(_));
        let closes = matches!(event, Event::End(_));
        let rule = matches!(event, Event::Rule);
        if depth == 0
            && (opens || rule)
            && let Some(rows) = ended.take()
            && let Some(at) = blank_cut(clean, range.start)
        {
            cut = Some((at, rows));
        }
        renderer.event(event);
        if opens {
            depth += 1;
        } else if closes {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                ended = Some(renderer.root_rows());
            }
        } else if rule && depth == 0 {
            ended = Some(renderer.root_rows());
        }
    }
    (renderer.finish(), cut)
}

/// Where to cut before a top-level block starting at `start`: the start of the blank line(s) in
/// front of it. `None` unless the block starts a complete line preceded by at least one blank line
/// with content before that.
fn blank_cut(clean: &str, start: usize) -> Option<usize> {
    let start = start.min(clean.len());
    if !clean[start..].contains('\n') {
        return None; // the first line of the next block may still turn into something else
    }
    let line_start = clean[..start].rfind('\n').map_or(0, |i| i + 1);
    if line_start == 0 || !clean[line_start..start].trim().is_empty() {
        return None;
    }
    let mut cut = line_start;
    let mut blanks = 0;
    while cut > 0 {
        let before = &clean[..cut - 1];
        let prev = before.rfind('\n').map_or(0, |i| i + 1);
        if !before[prev..].trim().is_empty() {
            break;
        }
        cut = prev;
        blanks += 1;
    }
    (blanks > 0 && cut > 0).then_some(cut)
}

/// Whether `line` may be a link reference definition (`[label]: destination`), which changes how
/// earlier text renders. Over-approximates (also inside code), which only costs a full re-render.
pub(crate) fn has_reference_definition(line: &str) -> bool {
    let mut t = line.trim_start_matches(' ');
    loop {
        let stripped = t
            .strip_prefix('>')
            .or_else(|| ["- ", "* ", "+ "].iter().find_map(|m| t.strip_prefix(m)))
            .or_else(|| {
                let digits = t.bytes().take_while(u8::is_ascii_digit).count();
                (digits > 0)
                    .then(|| {
                        t[digits..]
                            .strip_prefix(". ")
                            .or(t[digits..].strip_prefix(") "))
                    })
                    .flatten()
            });
        match stripped {
            Some(rest) => t = rest.trim_start_matches(' '),
            None => break,
        }
    }
    t.strip_prefix('[')
        .and_then(|rest| rest.find("]:"))
        .is_some_and(|close| close > 0)
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
    /// Open code block: trimmed fence info string and accumulated text.
    code: Option<(String, String)>,
    table: Option<TableBuf>,
}

const MIN_CODE_GUTTER: usize = 8;
/// Fence language that renders as a framed diagram panel: ```` ```diagram Optional title ````.
const DIAGRAM_LANG: &str = "diagram";
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

    /// Rows finished at the top level so far (only meaningful between top-level blocks).
    fn root_rows(&self) -> usize {
        self.frames.first().map_or(0, |f| f.rows.len())
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
                let info = match kind {
                    CodeBlockKind::Fenced(info) => info.trim().to_owned(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((info, String::new()));
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
                if let Some((info, body)) = self.code.take() {
                    self.code_block(&info, &body);
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
        if let Some((info, body)) = self.code.take() {
            self.code_block(&info, &body);
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

    fn code_block(&mut self, info: &str, body: &str) {
        let avail = self.avail();
        let theme = self.theme;
        let lang = fence_lang(info);
        if lang.eq_ignore_ascii_case(DIAGRAM_LANG) && avail >= MIN_CODE_GUTTER {
            let title = info[lang.len()..].trim();
            let rows = diagram_rows(theme, title, body, avail);
            self.begin_block();
            self.top().rows.extend(rows);
            return;
        }
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
        if avail < MIN_CODE_GUTTER {
            for line in lines {
                rows.extend(text::wrap(vec![line], avail));
            }
        } else {
            let bar = theme.sym(Sym::Quote);
            let gutter = theme.fg(Tok::MdCodeBorder);
            let inner = avail - 2;
            let label = text::ellipsize(lang, inner, theme.ascii());
            if !label.is_empty() {
                rows.push(Line::from(vec![
                    Span::styled(format!("{bar} "), gutter),
                    chrome::label(theme, &label),
                ]));
            }
            for line in lines {
                for wrapped in text::wrap(vec![line], inner) {
                    let mut spans = vec![Span::styled(format!("{bar} "), gutter)];
                    spans.extend(wrapped.spans);
                    rows.push(Line::from(spans));
                }
            }
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

/// The language word of a fence info string (`rust` in `rust,ignore` or `rust {.class}`).
fn fence_lang(info: &str) -> &str {
    info.split(|c: char| c.is_whitespace() || c == ',' || c == '{')
        .next()
        .unwrap_or("")
}

/// A diagram fence as a framed panel: an accent border with the title in its top edge, connector
/// glyphs tinted and labels in the code color, all on the code background. Rows are clipped with
/// an ellipsis, never wrapped, because a wrapped diagram no longer lines up. The full source stays
/// in the transcript. Callers guarantee `avail >= MIN_CODE_GUTTER`.
fn diagram_rows(theme: &Theme, title: &str, body: &str, avail: usize) -> Rows {
    let border = theme.fg(Tok::BorderAccent);
    let fill = theme.bg(Tok::MdCodeBlockBg);
    let h = theme.sym(Sym::BoxH);
    let v = theme.sym(Sym::BoxV);
    let ellipsis = theme.sym(Sym::Ellipsis);
    let body = body.strip_suffix('\n').unwrap_or(body);
    let lines: Vec<&str> = if body.is_empty() {
        Vec::new()
    } else {
        body.split('\n').collect()
    };
    // A row is `│ ` + inner + ` │`; the top edge is `╭─ ` + label + ` ` + rule (at least one) + `╮`.
    let max_inner = avail - 4;
    let (label_text, label_style) = if title.is_empty() {
        (DIAGRAM_LANG.to_uppercase(), theme.fg(Tok::Muted))
    } else {
        (
            title.to_owned(),
            theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
        )
    };
    let content_w = lines.iter().map(|l| text::width(l)).max().unwrap_or(0);
    let inner = content_w.max(text::width(&label_text) + 2).min(max_inner);
    let label = text::ellipsize(&label_text, inner - 2, theme.ascii());
    let rule_w = inner - 1 - text::width(&label);
    let mut rows: Rows = vec![Line::from(vec![
        Span::styled(format!("{}{h} ", theme.sym(Sym::BoxTl)), border),
        Span::styled(label, label_style),
        Span::styled(
            format!(" {}{}", h.repeat(rule_w), theme.sym(Sym::BoxTr)),
            border,
        ),
    ])];
    for line in lines {
        let mut content = diagram_spans(theme, line);
        if text::width(line) > inner {
            content = truncate_spans(content, inner, ellipsis);
        }
        let used: usize = content.iter().map(|s| text::width(&s.content)).sum();
        let mut spans = vec![Span::styled(v.to_owned(), border), Span::styled(" ", fill)];
        spans.extend(content.into_iter().map(|mut s| {
            s.style = s.style.patch(fill);
            s
        }));
        spans.push(Span::styled(" ".repeat(inner - used + 1), fill));
        spans.push(Span::styled(v.to_owned(), border));
        rows.push(Line::from(spans));
    }
    rows.push(Line::from(Span::styled(
        format!(
            "{}{}{}",
            theme.sym(Sym::BoxBl),
            h.repeat(inner + 2),
            theme.sym(Sym::BoxBr)
        ),
        border,
    )));
    rows
}

/// Diagram text with connectors (box drawing, arrows, `-->`, `|`, `[ ]`) in the accent color and
/// everything else in the code color, so node labels stand apart from the lines between them.
fn diagram_spans(theme: &Theme, line: &str) -> Vec<Span<'static>> {
    let (wire, label) = (theme.fg(Tok::Accent), theme.fg(Tok::MdCodeBlock));
    let graphemes: Vec<&str> = line.graphemes(true).collect();
    let first = |i: usize| graphemes.get(i).and_then(|g| g.chars().next());
    let mut spans = Vec::new();
    for (i, g) in graphemes.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(first);
        let connector = g
            .chars()
            .next()
            .is_some_and(|c| is_connector(prev, c, first(i + 1)));
        push_merged(&mut spans, g, if connector { wire } else { label });
    }
    spans
}

/// Whether `c`, between `prev` and `next`, draws part of a diagram rather than a label. Hyphens,
/// equals signs and underscores count only beside another line glyph, so `dial-app` and
/// `snake_case` stay labels while `-->` and `\___/` are drawn as connectors.
fn is_connector(prev: Option<char>, c: char, next: Option<char>) -> bool {
    let joins = |n: Option<char>| matches!(n, Some('-' | '=' | '<' | '>' | '+' | '|'));
    let word = |n: Option<char>| n.is_some_and(char::is_alphanumeric);
    let slope = |n: Option<char>| matches!(n, Some('_' | '/' | '\\'));
    match c {
        '|' | '+' | '<' | '>' | '^' | '[' | ']' => true,
        '-' | '=' => joins(prev) || joins(next),
        '_' => slope(prev) || slope(next),
        '/' | '\\' => !word(prev) && !word(next),
        // arrows, box drawing, block elements, geometric shapes, supplemental arrows
        '\u{2190}'..='\u{21FF}'
        | '\u{2500}'..='\u{25FF}'
        | '\u{27F0}'..='\u{27FF}'
        | '\u{2900}'..='\u{297F}' => true,
        _ => false,
    }
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
mod tests;
