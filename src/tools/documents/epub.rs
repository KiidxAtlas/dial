//! Bounded EPUB chapter extraction. The package is read through the shared document package
//! validator. Chapters follow the OPF spine and become plain text under `[chapter N: title]`
//! headers. Scripts, styles and head metadata never reach the output.
use super::MAX_CONTENT;
use super::archives;
use super::office::{Package, Xml, attr, xml};
use std::{borrow::Cow, collections::HashMap, fs::File, ops::RangeInclusive};

const CONTAINER: &str = "META-INF/container.xml";
const SELECTOR: &str = "chapter=";
const SKIPPED: &[&str] = &["head", "script", "style"];
const HEADINGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];
const BLOCKS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "html",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tr",
    "ul",
];
/// HTML's Latin-1 named entities, indexed from U+00A0 (`nbsp`).
const LATIN1: [&str; 96] = [
    "nbsp", "iexcl", "cent", "pound", "curren", "yen", "brvbar", "sect", "uml", "copy", "ordf",
    "laquo", "not", "shy", "reg", "macr", "deg", "plusmn", "sup2", "sup3", "acute", "micro",
    "para", "middot", "cedil", "sup1", "ordm", "raquo", "frac14", "frac12", "frac34", "iquest",
    "Agrave", "Aacute", "Acirc", "Atilde", "Auml", "Aring", "AElig", "Ccedil", "Egrave", "Eacute",
    "Ecirc", "Euml", "Igrave", "Iacute", "Icirc", "Iuml", "ETH", "Ntilde", "Ograve", "Oacute",
    "Ocirc", "Otilde", "Ouml", "times", "Oslash", "Ugrave", "Uacute", "Ucirc", "Uuml", "Yacute",
    "THORN", "szlig", "agrave", "aacute", "acirc", "atilde", "auml", "aring", "aelig", "ccedil",
    "egrave", "eacute", "ecirc", "euml", "igrave", "iacute", "icirc", "iuml", "eth", "ntilde",
    "ograve", "oacute", "ocirc", "otilde", "ouml", "divide", "oslash", "ugrave", "uacute", "ucirc",
    "uuml", "yacute", "thorn", "yuml",
];
/// Common HTML named entities outside Latin-1 (general punctuation, letters, arrows, symbols).
const EXTRA: &[(&str, u32)] = &[
    ("OElig", 338),
    ("oelig", 339),
    ("Scaron", 352),
    ("scaron", 353),
    ("Yuml", 376),
    ("fnof", 402),
    ("circ", 710),
    ("tilde", 732),
    ("ensp", 8194),
    ("emsp", 8195),
    ("thinsp", 8201),
    ("zwnj", 8204),
    ("zwj", 8205),
    ("lrm", 8206),
    ("rlm", 8207),
    ("ndash", 8211),
    ("mdash", 8212),
    ("lsquo", 8216),
    ("rsquo", 8217),
    ("sbquo", 8218),
    ("ldquo", 8220),
    ("rdquo", 8221),
    ("bdquo", 8222),
    ("dagger", 8224),
    ("Dagger", 8225),
    ("bull", 8226),
    ("hellip", 8230),
    ("permil", 8240),
    ("prime", 8242),
    ("Prime", 8243),
    ("lsaquo", 8249),
    ("rsaquo", 8250),
    ("oline", 8254),
    ("frasl", 8260),
    ("euro", 8364),
    ("trade", 8482),
    ("larr", 8592),
    ("uarr", 8593),
    ("rarr", 8594),
    ("darr", 8595),
    ("harr", 8596),
    ("minus", 8722),
    ("hearts", 9829),
];

/// Read the whole book, or one chapter when `selector` is `chapter=N` (1-based spine position).
pub(super) fn read(file: File, selector: Option<&str>) -> Result<String, String> {
    let wanted = selector.map(parse_selector).transpose()?;
    let mut package = Package::open(file)?;
    if !package.has(CONTAINER) {
        return Err("EPUB has no META-INF/container.xml; it is not a valid EPUB".into());
    }
    let opf_path = rootfile(&package.xml(CONTAINER)?)?;
    if !package.has(&opf_path) {
        return Err(format!(
            "EPUB container names a package document that is missing: {opf_path:?}"
        ));
    }
    let (manifest, spine) = manifest_and_spine(&package.xml(&opf_path)?)?;
    if spine.is_empty() {
        return Err("EPUB spine lists no chapters".into());
    }
    let base = opf_path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let chapters = spine
        .iter()
        .map(|idref| {
            let href = manifest
                .get(idref)
                .ok_or_else(|| format!("EPUB spine item {idref:?} has no manifest entry"))?;
            package_path(base, href)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let count = chapters.len();
    let range: RangeInclusive<usize> = match wanted {
        Some(n) if n > count => return Err(format!("chapter index outside 1..={count}")),
        Some(n) => n..=n,
        None => 1..=count,
    };
    let mut out = String::new();
    for number in range {
        let path = &chapters[number - 1];
        if !package.has(path) {
            return Err(format!("EPUB chapter {path:?} is missing from the package"));
        }
        let (title, body) = convert(&package.xml(path)?, stem(path))?;
        let block = if body.is_empty() {
            format!("[chapter {number}: {title}]")
        } else {
            format!("[chapter {number}: {title}]\n{body}")
        };
        let separator = if out.is_empty() { "" } else { "\n\n" };
        if out.len() + separator.len() + block.len() > MAX_CONTENT as usize {
            return Err("EPUB extracted text exceeds 8 MiB; select :chapter=N".into());
        }
        out.push_str(separator);
        out.push_str(&block);
    }
    Ok(out)
}

fn parse_selector(member: &str) -> Result<usize, String> {
    let index = member
        .strip_prefix(SELECTOR)
        .ok_or("epub selector must be :chapter=N (1-based)")?;
    match index.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err("invalid chapter index".into()),
    }
}

/// The package document path named by `META-INF/container.xml`.
fn rootfile(container: &str) -> Result<String, String> {
    let mut found = None;
    xml(container, "container", |event| {
        if let Xml::Start(name, attrs) = event
            && name == "rootfile"
            && found.is_none()
        {
            found = attr(&attrs, "full-path").map(str::to_owned);
        }
        Ok(())
    })?;
    let path = found.ok_or("EPUB container.xml names no rootfile full-path")?;
    if !archives::safe(&path) {
        return Err(format!("unsafe EPUB package path {path:?}"));
    }
    Ok(path)
}

/// Manifest (id → href) and spine (ordered idrefs) of the package document.
fn manifest_and_spine(opf: &str) -> Result<(HashMap<String, String>, Vec<String>), String> {
    let mut manifest = HashMap::new();
    let mut spine = Vec::new();
    xml(opf, "package", |event| {
        if let Xml::Start(name, attrs) = event {
            match name.as_str() {
                "item" => {
                    let id = attr(&attrs, "id").ok_or("EPUB manifest item has no id")?;
                    let href = attr(&attrs, "href").ok_or("EPUB manifest item has no href")?;
                    if manifest.insert(id.to_owned(), href.to_owned()).is_some() {
                        return Err(format!("EPUB manifest repeats id {id:?}"));
                    }
                }
                "itemref" => {
                    let idref = attr(&attrs, "idref").ok_or("EPUB spine itemref has no idref")?;
                    spine.push(idref.to_owned());
                }
                _ => {}
            }
        }
        Ok(())
    })?;
    Ok((manifest, spine))
}

/// Resolve a manifest href against the package directory, rejecting anything that leaves it.
fn package_path(base_dir: &str, href: &str) -> Result<String, String> {
    if href.starts_with('/') || href.contains(':') {
        return Err(format!(
            "EPUB manifest href {href:?} must be relative to the package"
        ));
    }
    let relative = href.split(['#', '?']).next().unwrap_or_default();
    let decoded = percent_decode(relative)?;
    let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in decoded.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts
                    .pop()
                    .ok_or_else(|| format!("EPUB manifest href {href:?} escapes the package"))?;
            }
            name => parts.push(name),
        }
    }
    let path = parts.join("/");
    if path.is_empty() || !archives::safe(&path) {
        return Err(format!("unsafe EPUB manifest href {href:?}"));
    }
    Ok(path)
}

fn percent_decode(input: &str) -> Result<String, String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let value = bytes
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .ok_or_else(|| format!("invalid percent escape in EPUB href {input:?}"))?;
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("EPUB href {input:?} is not UTF-8"))
}

/// The file stem of a package path, used as a chapter title when the chapter has no heading.
fn stem(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.rsplit_once('.').map_or(file, |(stem, _)| stem)
}

/// Rewrite HTML named entities that XML does not define into numeric references, so the strict
/// XML reader can decode them. Unknown named entities are rejected with their name.
fn normalize_entities(input: &str) -> Result<Cow<'_, str>, String> {
    if !input.contains('&') {
        return Ok(Cow::Borrowed(input));
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let name = tail
            .bytes()
            .take(34)
            .position(|b| b == b';')
            .map(|end| &tail[1..end])
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'#'));
        match name {
            Some(name) => {
                if name.starts_with('#') || matches!(name, "amp" | "lt" | "gt" | "quot" | "apos") {
                    out.push_str(&tail[..name.len() + 2]);
                } else {
                    let code = named_entity(name).ok_or_else(|| {
                        format!("unsupported HTML entity &{name}; in EPUB chapter")
                    })?;
                    out.push_str(&format!("&#{code};"));
                }
                rest = &tail[name.len() + 2..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    Ok(Cow::Owned(out))
}

fn named_entity(name: &str) -> Option<u32> {
    if let Some(index) = LATIN1.iter().position(|n| *n == name) {
        return Some(160 + index as u32);
    }
    EXTRA
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, code)| *code)
}

/// Plain text of one XHTML chapter as `(title, body)`. The title is the first heading, then the
/// document `<title>`, then `fallback`.
fn convert(html: &str, fallback: &str) -> Result<(String, String), String> {
    let html = normalize_entities(html)?;
    let mut chapter = Chapter::default();
    xml(&html, "html", |event| {
        match event {
            Xml::Start(name, _) => chapter.start(&name),
            Xml::End(name) => chapter.end(&name),
            Xml::Text(value) => chapter.text(&value),
        }
        Ok(())
    })?;
    let title = chapter
        .first_heading
        .or(chapter.head_title)
        .unwrap_or_else(|| fallback.to_owned());
    Ok((title, chapter.body.trim_end().to_owned()))
}

#[derive(Default)]
struct Chapter {
    body: String,
    pending_space: bool,
    /// Depth inside skipped elements (`head`, `script`, `style`).
    skip: usize,
    headings: usize,
    heading: String,
    first_heading: Option<String>,
    head_title: Option<String>,
    in_title: bool,
    title: String,
}

impl Chapter {
    fn start(&mut self, name: &str) {
        if name == "title" {
            self.in_title = true;
            self.title.clear();
            return;
        }
        if SKIPPED.contains(&name) {
            self.skip += 1;
            return;
        }
        if self.skip > 0 {
            return;
        }
        if HEADINGS.contains(&name) {
            self.headings += 1;
        }
        if BLOCKS.contains(&name) {
            self.line_break();
            if name == "li" {
                self.body.push_str("- ");
            }
        } else if name == "td" || name == "th" {
            self.pending_space = true;
        }
    }

    fn end(&mut self, name: &str) {
        if name == "title" {
            self.in_title = false;
            let title = collapse(&self.title);
            if self.head_title.is_none() && !title.is_empty() {
                self.head_title = Some(title);
            }
            return;
        }
        if SKIPPED.contains(&name) {
            self.skip = self.skip.saturating_sub(1);
            return;
        }
        if self.skip > 0 {
            return;
        }
        if HEADINGS.contains(&name) {
            self.headings = self.headings.saturating_sub(1);
            if self.headings == 0 {
                let heading = collapse(&std::mem::take(&mut self.heading));
                if self.first_heading.is_none() && !heading.is_empty() {
                    self.first_heading = Some(heading);
                }
            }
        }
        if BLOCKS.contains(&name) {
            self.line_break();
        }
    }

    fn text(&mut self, value: &str) {
        if self.in_title {
            self.title.push_str(value);
            return;
        }
        if self.skip > 0 {
            return;
        }
        if self.headings > 0 {
            self.heading.push_str(value);
        }
        for ch in value.chars() {
            if ch.is_whitespace() {
                self.pending_space = true;
            } else {
                if self.pending_space && !self.body.is_empty() && !self.body.ends_with(['\n', ' '])
                {
                    self.body.push(' ');
                }
                self.pending_space = false;
                self.body.push(ch);
            }
        }
    }

    fn line_break(&mut self) {
        if !self.body.is_empty() && !self.body.ends_with('\n') {
            self.body.push('\n');
        }
        self.pending_space = false;
    }
}

fn collapse(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::io::{Seek, Write};

    const CONTAINER_XML: &str = r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
    /// Manifest order differs from spine order, so the tests prove that the spine sets reading order.
    const OPF_XML: &str = r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/><manifest><item id="ch2" href="text/second.xhtml" media-type="application/xhtml+xml"/><item id="ch1" href="first.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="ch1"/><itemref idref="ch2"/></spine></package>"#;

    fn chapter_xhtml(title: &str, body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE html><html xmlns="http://www.w3.org/1999/xhtml"><head><title>{title}</title><style>p {{ color: red; }}</style></head><body>{body}</body></html>"#
        )
    }

    fn package(files: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// A two-chapter EPUB whose chapter bodies are supplied by the caller.
    fn book_with(first_body: &str, second_body: &str) -> Vec<u8> {
        let first = chapter_xhtml("ignored", first_body);
        let second = chapter_xhtml("Second", second_body);
        package(&[
            ("mimetype", "application/epub+zip"),
            ("META-INF/container.xml", CONTAINER_XML),
            ("OEBPS/content.opf", OPF_XML),
            ("OEBPS/first.xhtml", &first),
            ("OEBPS/text/second.xhtml", &second),
        ])
    }

    /// The fixture used by the tool-path tests: heading, markup, script/style and a list.
    pub(in crate::tools::documents) fn sample_epub() -> Vec<u8> {
        book_with(
            "<h1>First &amp; Last</h1><p>Hello <em>world</em>.</p><script>evil()</script>",
            "<h2>Second chapter</h2><ul><li>alpha</li><li>beta</li></ul>",
        )
    }

    fn read_bytes(bytes: &[u8], selector: Option<&str>) -> Result<String, String> {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        read(file, selector)
    }

    #[test]
    fn spine_order_drives_chapter_numbers_and_scripts_are_dropped() {
        let all = read_bytes(&sample_epub(), None).unwrap();
        let first = all
            .find("[chapter 1: First & Last]")
            .expect("chapter 1 header");
        let second = all
            .find("[chapter 2: Second chapter]")
            .expect("chapter 2 header");
        assert!(first < second, "{all}");
        assert!(all.contains("Hello world."), "{all}");
        assert!(all.contains("- alpha\n- beta"), "{all}");
        assert!(
            !all.contains("evil()") && !all.contains("color: red"),
            "{all}"
        );
        assert!(!all.contains("ignored"), "{all}");
    }

    #[test]
    fn chapter_selector_returns_only_that_spine_position() {
        let second = read_bytes(&sample_epub(), Some("chapter=2")).unwrap();
        assert!(
            second.starts_with("[chapter 2: Second chapter]"),
            "{second}"
        );
        assert!(
            !second.contains("Hello") && !second.contains("[chapter 1"),
            "{second}"
        );
        let first = read_bytes(&sample_epub(), Some("chapter=1")).unwrap();
        assert!(first.starts_with("[chapter 1: First & Last]"), "{first}");
    }

    #[test]
    fn out_of_range_and_malformed_selectors_are_actionable_errors() {
        let bytes = sample_epub();
        let past = read_bytes(&bytes, Some("chapter=3")).unwrap_err();
        assert!(past.contains("chapter index outside 1..=2"), "{past}");
        for bad in ["chapter=0", "chapter=x", "chapter="] {
            let error = read_bytes(&bytes, Some(bad)).unwrap_err();
            assert!(error.contains("invalid chapter index"), "{bad}: {error}");
        }
        let wrong = read_bytes(&bytes, Some("cell=1")).unwrap_err();
        assert!(wrong.contains("must be :chapter=N"), "{wrong}");
    }

    #[test]
    fn missing_container_is_reported_clearly() {
        let bytes = package(&[("OEBPS/content.opf", OPF_XML)]);
        let error = read_bytes(&bytes, None).unwrap_err();
        assert!(error.contains("META-INF/container.xml"), "{error}");
    }

    #[test]
    fn entity_heavy_chapter_decodes_html_and_numeric_entities() {
        let bytes = book_with(
            "<p>caf&eacute; &nbsp;&mdash; &#8212; &#x2014; &amp; &lt;tag&gt; &quot;q&quot; &hellip;</p>",
            "<p>plain</p>",
        );
        let all = read_bytes(&bytes, Some("chapter=1")).unwrap();
        assert!(
            all.lines().any(|line| line == "café — — — & <tag> \"q\" …"),
            "{all}"
        );
    }

    #[test]
    fn unknown_named_entities_and_internal_dtds_are_rejected() {
        let error =
            read_bytes(&book_with("<p>&bogus;</p>", "<p>x</p>"), Some("chapter=1")).unwrap_err();
        assert!(error.contains("&bogus;"), "{error}");
        let dtd = convert(
            "<!DOCTYPE html [<!ENTITY x \"y\">]><html><body>x</body></html>",
            "t",
        )
        .unwrap_err();
        assert!(dtd.contains("DTDs"), "{dtd}");
    }

    #[test]
    fn unsafe_member_names_and_escaping_hrefs_are_rejected() {
        let evil = package(&[
            ("META-INF/container.xml", CONTAINER_XML),
            ("OEBPS/content.opf", OPF_XML),
            ("../evil.xhtml", "x"),
        ]);
        let error = read_bytes(&evil, None).unwrap_err();
        assert!(error.contains("unsafe or duplicate"), "{error}");

        let escaping_opf = OPF_XML.replace("first.xhtml", "../../outside.xhtml");
        let escaping = package(&[
            ("META-INF/container.xml", CONTAINER_XML),
            ("OEBPS/content.opf", &escaping_opf),
        ]);
        let error = read_bytes(&escaping, None).unwrap_err();
        assert!(error.contains("escapes the package"), "{error}");
    }

    #[test]
    fn package_paths_resolve_relative_hrefs_and_reject_the_rest() {
        assert_eq!(
            package_path("OEBPS", "text/a%20b.xhtml#frag").unwrap(),
            "OEBPS/text/a b.xhtml"
        );
        assert_eq!(
            package_path("OEBPS/text", "../ch.xhtml").unwrap(),
            "OEBPS/ch.xhtml"
        );
        assert_eq!(package_path("", "ch.xhtml").unwrap(), "ch.xhtml");
        assert!(package_path("OEBPS", "../../x.xhtml").is_err());
        assert!(package_path("OEBPS", "/abs.xhtml").is_err());
        assert!(package_path("OEBPS", "https://example.com/x.xhtml").is_err());
        assert!(package_path("OEBPS", "bad%zz.xhtml").is_err());
    }

    #[test]
    fn entity_normalization_keeps_xml_entities_and_bare_ampersands() {
        assert_eq!(normalize_entities("a &amp; b").unwrap(), "a &amp; b");
        assert_eq!(normalize_entities("&nbsp;").unwrap(), "&#160;");
        assert_eq!(normalize_entities("x & y").unwrap(), "x & y");
        assert_eq!(normalize_entities("&#x2014;").unwrap(), "&#x2014;");
        assert!(normalize_entities("&nope;").is_err());
    }

    #[test]
    fn heading_then_title_then_fallback_names_each_chapter() {
        let (title, body) = convert(
            "<html><head><title>Doc</title></head><body><p>x</p></body></html>",
            "fallback",
        )
        .unwrap();
        assert_eq!((title.as_str(), body.as_str()), ("Doc", "x"));
        let (title, _) = convert("<html><body><p>x</p></body></html>", "fallback").unwrap();
        assert_eq!(title, "fallback");
    }
}
