//! Renders an email's `text/html` part as Telegram-safe HTML.
//!
//! "Read all" used to forward the sender's `text/plain` part verbatim. For
//! a marketing mail (Duolingo, a receipt, a newsletter) that part is an
//! auto-generated afterthought: the reader got literal `**bold**`, a
//! seven-column streak table flattened to one cell per line, stray `----`
//! rules and a raw tracking URL. The HTML part is the one the sender
//! actually designed, so it is the one we convert.
//!
//! This module deliberately does not try to be a browser: it walks the
//! markup, drops what cannot or should not be shown in a chat, and emits a
//! small fixed subset of Telegram's HTML tags (`b`, `i`, `a`, plus
//! newlines). That subset is what makes the output safe to send with
//! `parse_mode: HTML` on attacker-controlled mail: text is escaped, link
//! targets are restricted to a handful of schemes, and every tag we emit is
//! closed again - unlike MarkdownV2, where a stray `_` in an address is
//! enough for Telegram to reject the whole message.
//!
//! Dropped on purpose: images (their `alt` only survives inside a link),
//! scripts, styles, `<head>`, comments, and anything the sender hid with
//! `display:none`-style CSS - marketing mail starts with a hidden
//! "preheader" sentence that would otherwise be the first line of every
//! message.

use std::borrow::Cow;

/// A run of text that shares one style. `text` may contain newlines; the
/// renderer keeps them (they are the only block structure Telegram has).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub link: Option<String>,
}

/// Block-level tags: a line break before and after their content. Email
/// clients nest these for layout, which is exactly why the text part of a
/// marketing mail comes out with one word per line - here they only ever
/// cost a newline, and repeated newlines are collapsed to one.
const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "dd",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "tbody",
    "tfoot",
    "thead",
    "tr",
    "ul",
];

/// Content that is never readable text: skip containers (their children can
/// still be skipped one by one, so nesting counters stay balanced).
const SKIP_CONTAINERS: &[&str] = &[
    "head", "noscript", "template", "svg", "iframe", "object", "canvas",
];

/// Content that is raw text, not markup: skipping to the closing tag avoids
/// interpreting a CSS selector like `a > b` as HTML.
const RAW_TEXT_TAGS: &[&str] = &["script", "style", "title"];

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// CSS that means "do not show this to a human". Marketing mail hides its
/// preheader with one of these; keeping the list short and specific avoids
/// dropping content that merely happens to be styled.
const HIDDEN_MARKERS: &[&str] = &[
    "display:none",
    "visibility:hidden",
    "max-height:0",
    "mso-hide:all",
];

/// Longest link target we will put in an `href`. Real tracking URLs fit
/// comfortably; anything past this is more likely to be corrupt than
/// useful, and Telegram counts the attribute when it parses the message.
const MAX_LINK_LEN: usize = 2048;

/// Converts an HTML fragment into styled spans. Never panics: malformed
/// markup degrades to literal text instead of an error, because the input
/// is whatever arrived in the mailbox.
pub fn spans_from_html(html: &str) -> Vec<Span> {
    let mut converter = Converter::new(html);
    while converter.pos < html.len() {
        if converter.src[converter.pos..].starts_with("<!--") {
            match converter.src[converter.pos..].find("-->") {
                Some(offset) => converter.pos += offset + 3,
                None => converter.pos = converter.src.len(),
            }
            continue;
        }
        if converter.src.as_bytes()[converter.pos] == b'<' {
            converter.tag();
        } else {
            let next = converter.src[converter.pos..]
                .find('<')
                .map(|offset| converter.pos + offset)
                .unwrap_or(converter.src.len());
            // The slice borrows `src` while `push_text` needs `&mut self`,
            // so hand over an owned run. Text runs are short next to the
            // markup they sit in.
            let text = converter.src[converter.pos..next].to_string();
            converter.push_text(&text);
            converter.pos = next;
        }
    }
    converter.finish()
}

/// Renders spans as Telegram HTML, split into chunks of at most `max_len`
/// UTF-16 code units of *visible text* (Telegram's unit - see
/// `format::chunk_text`). Tags do not count towards the budget; each chunk
/// is independently well-formed, with any open `b`/`i`/`a` reopened at the
/// start of the next chunk and closed again at its end.
pub fn chunk_spans(spans: &[Span], max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    if max_len == 0 {
        return chunks;
    }
    let mut current = String::new();
    let mut used = 0usize;
    let mut has_text = false;

    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let mut opened = false;
        for c in span.text.chars() {
            // Never begin a chunk with a blank line left over from the
            // previous one.
            if !has_text && c == '\n' {
                continue;
            }
            let units = c.len_utf16();
            if used + units > max_len {
                if opened {
                    close_markup(&mut current, span);
                    opened = false;
                }
                if has_text {
                    chunks.push(std::mem::take(&mut current));
                }
                used = 0;
                has_text = false;
                if c == '\n' {
                    continue;
                }
            }
            if !opened {
                open_markup(&mut current, span);
                opened = true;
            }
            push_escaped(&mut current, c);
            used += units;
            has_text = true;
        }
        if opened {
            close_markup(&mut current, span);
        }
    }
    if has_text {
        chunks.push(current);
    }
    chunks
}

fn open_markup(out: &mut String, span: &Span) {
    if span.bold {
        out.push_str("<b>");
    }
    if span.italic {
        out.push_str("<i>");
    }
    if let Some(link) = &span.link {
        out.push_str("<a href=\"");
        push_escaped_attr(out, link);
        out.push_str("\">");
    }
}

fn close_markup(out: &mut String, span: &Span) {
    if span.link.is_some() {
        out.push_str("</a>");
    }
    if span.italic {
        out.push_str("</i>");
    }
    if span.bold {
        out.push_str("</b>");
    }
}

fn push_escaped(out: &mut String, c: char) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        _ => out.push(c),
    }
}

fn push_escaped_attr(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

struct OpenTag {
    name: String,
    skip: bool,
    hidden: bool,
}

struct Converter<'a> {
    src: &'a str,
    pos: usize,
    spans: Vec<Span>,
    text: String,
    bold: usize,
    italic: usize,
    link: Option<String>,
    link_has_text: bool,
    space_pending: bool,
    /// A block boundary was seen while the current span was empty (a style
    /// change had just flushed it); the newline has to be carried over to
    /// the next span or two blocks would run together.
    break_pending: bool,
    skip_depth: usize,
    hidden: usize,
    stack: Vec<OpenTag>,
    table_depth: usize,
    cell_text: bool,
}

impl<'a> Converter<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            spans: Vec::new(),
            text: String::new(),
            bold: 0,
            italic: 0,
            link: None,
            link_has_text: false,
            space_pending: false,
            break_pending: false,
            skip_depth: 0,
            hidden: 0,
            stack: Vec::new(),
            table_depth: 0,
            cell_text: false,
        }
    }

    fn suppressed(&self) -> bool {
        self.skip_depth > 0 || self.hidden > 0
    }

    /// Parses one `<...>` starting at `pos`, or treats the `<` as text when
    /// no tag ends before EOF.
    fn tag(&mut self) {
        let bytes = self.src.as_bytes();
        let start = self.pos;
        let mut i = start + 1;
        let mut quote = 0u8;
        while i < bytes.len() {
            let b = bytes[i];
            if quote != 0 {
                if b == quote {
                    quote = 0;
                }
            } else if b == b'"' || b == b'\'' {
                quote = b;
            } else if b == b'>' {
                break;
            }
            i += 1;
        }
        if i >= bytes.len() {
            self.push_text("<");
            self.pos = start + 1;
            return;
        }
        let inner = self.src[start + 1..i].to_string();
        self.pos = i + 1;

        let trimmed = inner.trim();
        if trimmed.is_empty() || trimmed.starts_with('!') || trimmed.starts_with('?') {
            return;
        }
        if let Some(rest) = trimmed.strip_prefix('/') {
            let name = tag_name(rest);
            if !name.is_empty() {
                self.handle_end(name);
            }
            return;
        }
        let name = tag_name(trimmed);
        if name.is_empty() {
            return;
        }
        let self_closing = trimmed.ends_with('/');
        self.handle_start(name, &inner, self_closing);
    }

    fn handle_start(&mut self, name: &str, inner: &str, self_closing: bool) {
        let lower = name.to_ascii_lowercase();

        if RAW_TEXT_TAGS.contains(&lower.as_str()) {
            self.raw_skip(&lower);
            return;
        }

        let hidden_style = is_hidden(inner);
        let skip_container = SKIP_CONTAINERS.contains(&lower.as_str());
        let tracked = !VOID_TAGS.contains(&lower.as_str()) && !self_closing;
        if tracked {
            if skip_container {
                self.skip_depth += 1;
            }
            if hidden_style {
                self.hidden += 1;
            }
            self.stack.push(OpenTag {
                name: lower.clone(),
                skip: skip_container,
                hidden: hidden_style,
            });
        }

        if self.suppressed() {
            // Counters still have to move, or a formatting tag opened
            // before a hidden block would stay open after it.
            match lower.as_str() {
                "b" | "strong" => self.bold += 1,
                "i" | "em" => self.italic += 1,
                _ => {}
            }
            return;
        }

        match lower.as_str() {
            "b" | "strong" => {
                self.flush();
                self.bold += 1;
            }
            "i" | "em" => {
                self.flush();
                self.italic += 1;
            }
            "a" => {
                if let Some(href) = attr_value(inner, "href").and_then(valid_link) {
                    self.flush();
                    self.link = Some(href);
                    self.link_has_text = false;
                }
            }
            "img" => {
                // A button in marketing mail is often an image inside the
                // link; its alt text is the only visible label we have, so
                // keep that much and drop the image itself.
                if self.link.is_some()
                    && let Some(alt) = attr_value(inner, "alt")
                {
                    let alt = decode_entities(alt);
                    if !alt.trim().is_empty() {
                        self.push_text(alt.as_ref());
                    }
                }
            }
            "br" => {
                if self.table_depth > 0 {
                    self.push_text(" ");
                } else {
                    self.break_line();
                }
            }
            "hr" => self.break_line(),
            "li" => {
                self.break_line();
                self.push_text("• ");
            }
            "td" | "th" => self.cell_text = false,
            "table" => {
                self.table_depth += 1;
                self.break_line();
            }
            other if BLOCK_TAGS.contains(&other) => self.break_line(),
            _ => {}
        }
    }

    fn handle_end(&mut self, name: &str) {
        let lower = name.to_ascii_lowercase();

        // Pop through the matching opener: email HTML is machine-generated
        // and well-formed in practice, but an unclosed element must not
        // leave the hidden/skip counters stuck for the rest of the message.
        if let Some(index) = self.stack.iter().rposition(|tag| tag.name == lower) {
            while self.stack.len() > index {
                if let Some(tag) = self.stack.pop() {
                    if tag.skip {
                        self.skip_depth = self.skip_depth.saturating_sub(1);
                    }
                    if tag.hidden {
                        self.hidden = self.hidden.saturating_sub(1);
                    }
                }
            }
        }

        if self.suppressed() {
            match lower.as_str() {
                "b" | "strong" => self.bold = self.bold.saturating_sub(1),
                "i" | "em" => self.italic = self.italic.saturating_sub(1),
                "a" => self.link = None,
                _ => {}
            }
            return;
        }

        match lower.as_str() {
            "b" | "strong" => {
                self.flush();
                self.bold = self.bold.saturating_sub(1);
            }
            "i" | "em" => {
                self.flush();
                self.italic = self.italic.saturating_sub(1);
            }
            "a" => {
                if self.link.is_some() && !self.link_has_text {
                    // Nothing readable inside the anchor (an image-only
                    // button): keep the action reachable.
                    if let Some(href) = self.link.clone() {
                        self.push_text(&href);
                    }
                }
                self.flush();
                self.link = None;
                self.link_has_text = false;
            }
            "td" | "th" => {
                if self.cell_text {
                    self.push_text(" ");
                }
                self.cell_text = false;
            }
            "table" => {
                self.table_depth = self.table_depth.saturating_sub(1);
                self.break_line();
            }
            other if BLOCK_TAGS.contains(&other) => self.break_line(),
            _ => {}
        }
    }

    /// Skips the raw body of a `<script>`/`<style>`/`<title>` up to its
    /// closing tag. CSS (`a > b`) would otherwise parse as markup.
    fn raw_skip(&mut self, name: &str) {
        match find_closing_tag(&self.src[self.pos..], name) {
            Some(offset) => {
                let after = self.pos + offset;
                match self.src[after..].find('>') {
                    Some(end) => self.pos = after + end + 1,
                    None => self.pos = self.src.len(),
                }
            }
            None => self.pos = self.src.len(),
        }
    }

    fn push_text(&mut self, raw: &str) {
        if self.suppressed() {
            return;
        }
        let decoded = decode_entities(raw);
        for c in decoded.chars() {
            if is_invisible(c) {
                continue;
            }
            if c.is_whitespace() {
                self.space_pending = true;
                continue;
            }
            if self.break_pending {
                self.break_pending = false;
                self.space_pending = false;
                if !self.text.ends_with('\n') {
                    self.text.push('\n');
                }
            }
            if self.space_pending && !self.text.is_empty() && !self.text.ends_with('\n') {
                self.text.push(' ');
            }
            self.space_pending = false;
            self.text.push(c);
            if self.table_depth > 0 {
                self.cell_text = true;
            }
            if self.link.is_some() {
                self.link_has_text = true;
            }
        }
    }

    fn break_line(&mut self) {
        if self.suppressed() {
            return;
        }
        self.space_pending = false;
        if self.text.is_empty() {
            // Nothing to break yet: remember it for the next text run,
            // unless this is the start of the message.
            if !self.spans.is_empty() {
                self.break_pending = true;
            }
            return;
        }
        if !self.text.ends_with('\n') {
            self.text.push('\n');
        }
    }

    fn flush(&mut self) {
        if self.text.is_empty() {
            // A block boundary seen right after a style change belongs at
            // the end of the span just closed, not at the start of the
            // next one: otherwise a heading's newline would land inside
            // the bold text that follows it.
            if self.break_pending {
                self.break_pending = false;
                if let Some(last) = self.spans.last_mut()
                    && !last.text.ends_with('\n')
                {
                    last.text.push('\n');
                }
            }
            return;
        }
        let mut text = std::mem::take(&mut self.text);
        // A pending space belongs *before* the word that follows, which
        // may land in the next span; keep it on this side so merging or
        // chunking cannot glue two words together.
        if self.space_pending && !text.ends_with('\n') {
            text.push(' ');
        }
        self.space_pending = false;
        self.spans.push(Span {
            text,
            bold: self.bold > 0,
            italic: self.italic > 0,
            link: self.link.clone(),
        });
    }

    fn finish(mut self) -> Vec<Span> {
        self.flush();

        let mut merged: Vec<Span> = Vec::with_capacity(self.spans.len());
        for span in self.spans {
            if let Some(last) = merged.last_mut()
                && last.bold == span.bold
                && last.italic == span.italic
                && last.link == span.link
            {
                last.text.push_str(&span.text);
                continue;
            }
            merged.push(span);
        }

        if let Some(first) = merged.first_mut() {
            first.text = first
                .text
                .trim_start_matches(char::is_whitespace)
                .to_string();
        }
        if let Some(last) = merged.last_mut() {
            last.text = last.text.trim_end_matches(char::is_whitespace).to_string();
        }
        merged.retain(|span| !span.text.is_empty());
        merged
    }
}

fn tag_name(inner: &str) -> &str {
    let inner = inner.trim_start();
    let end = inner
        .find(|c: char| c.is_ascii_whitespace() || c == '/')
        .unwrap_or(inner.len());
    &inner[..end]
}

/// Reads an attribute value out of a tag's inner text (`href="..."`,
/// `style='...'`, bare `hidden`). Case-insensitive on the name.
fn attr_value<'a>(inner: &'a str, wanted: &str) -> Option<&'a str> {
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let name = &inner[name_start..i];
        if name.is_empty() {
            i += 1;
            continue;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let value_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                let value = &inner[value_start..i];
                if i < bytes.len() {
                    i += 1;
                }
                if name.eq_ignore_ascii_case(wanted) {
                    return Some(value);
                }
            } else {
                let value_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if name.eq_ignore_ascii_case(wanted) {
                    return Some(&inner[value_start..i]);
                }
            }
        } else if name.eq_ignore_ascii_case(wanted) {
            return Some("");
        }
    }
    None
}

fn is_hidden(inner: &str) -> bool {
    if attr_value(inner, "hidden").is_some() {
        return true;
    }
    let Some(style) = attr_value(inner, "style") else {
        return false;
    };
    let compact: String = style
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    HIDDEN_MARKERS.iter().any(|marker| compact.contains(marker))
}

/// Only schemes that make sense when tapped from a chat. A `javascript:`
/// or `data:` target is never rendered as a link.
fn valid_link(raw: &str) -> Option<String> {
    let decoded = decode_entities(raw).into_owned();
    let cleaned: String = decoded
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect();
    if cleaned.is_empty() || cleaned.len() > MAX_LINK_LEN {
        return None;
    }
    let lower = cleaned.to_ascii_lowercase();
    let known = ["http://", "https://", "mailto:", "tel:", "tg://"]
        .iter()
        .any(|scheme| lower.starts_with(scheme));
    known.then_some(cleaned)
}

/// Characters a sender uses as invisible padding (`&#847;`, `&zwnj;`,
/// zero-width spaces). They are not whitespace, so without this they would
/// survive as literal garbage in the message. ZWJ (U+200D) and the emoji
/// variation selectors are deliberately *not* stripped: they are what makes
/// multi-codepoint emoji render as one glyph.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{17b4}'
            | '\u{17b5}'
            | '\u{180b}'..='\u{180e}'
            | '\u{200b}'
            | '\u{200c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
    )
}

/// Finds `</name` case-insensitively without allocating.
///
/// The obvious implementation - lowercasing the whole remaining document
/// and searching it - is quadratic when the document repeats the tag: a
/// 256 KiB body of empty `<style></style>` pairs is ~37k skipped tags and
/// several gigabytes of copying, all on a tokio worker with no await
/// point. Everything here works on the existing bytes, so the scan stays
/// linear and allocation-free.
fn find_closing_tag(haystack: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(relative) = haystack[from..].find("</") {
        let at = from + relative;
        let name_start = at + 2;
        let name_end = name_start + name.len();
        if name_end <= haystack.len()
            && haystack[name_start..name_end].eq_ignore_ascii_case(name)
            && matches!(
                haystack.as_bytes().get(name_end),
                // The name has to end where the tag name does: `</stylex`
                // is a different tag, not our closing tag.
                None | Some(b'>')
                    | Some(b'/')
                    | Some(b' ')
                    | Some(b'\t')
                    | Some(b'\n')
                    | Some(b'\r')
            )
        {
            return Some(at);
        }
        from = at + 2;
    }
    None
}

fn decode_entities(input: &str) -> Cow<'_, str> {
    if !input.contains('&') {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let after = &rest[index..];
        // An entity name is short; a ';' much later is not an entity.
        let end = after.find(';').filter(|end| *end <= 32);
        match end {
            Some(end) => {
                match entity_value(&after[1..end]) {
                    Some(c) => out.push(c),
                    // Unknown named entity: keep it verbatim rather than
                    // silently deleting text.
                    None => out.push_str(&after[..=end]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

fn entity_value(name: &str) -> Option<char> {
    if let Some(digits) = name.strip_prefix('#') {
        let code = if let Some(hex) = digits.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            digits.parse::<u32>().ok()?
        };
        return char::from_u32(code);
    }
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" | "ensp" | "emsp" | "thinsp" => ' ',
        "zwnj" | "zwj" | "lrm" | "rlm" | "shy" => '\u{00ad}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "ndash" => '–',
        "mdash" => '—',
        "laquo" => '«',
        "raquo" => '»',
        "rsquo" | "lsquo" => '\'',
        "rdquo" | "ldquo" => '"',
        "bull" => '•',
        "middot" => '·',
        "times" => '×',
        "divide" => '÷',
        "deg" => '°',
        "euro" => '€',
        "eacute" => 'é',
        "egrave" => 'è',
        "ecirc" => 'ê',
        "agrave" => 'à',
        "acirc" => 'â',
        "ccedil" => 'ç',
        "ugrave" => 'ù',
        "ucirc" => 'û',
        "icirc" => 'î',
        "ocirc" => 'ô',
        "auml" => 'ä',
        "ouml" => 'ö',
        "uuml" => 'ü',
        "szlig" => 'ß',
        "aelig" => 'æ',
        "oeilg" => 'œ',
        _ => return None,
    };
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(html: &str) -> String {
        spans_from_html(html)
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    fn html_of(spans: &[Span]) -> String {
        chunk_spans(spans, usize::MAX).join("")
    }

    #[test]
    fn drops_the_hidden_preheader_and_the_head() {
        let html = r#"
            <html><head><title>Duolingo</title><style>a > b { color: red }</style></head>
            <body>
              <div style="display:none;max-height:0;overflow:hidden">Préheader invisible &nbsp; &#847;</div>
              <p>Commence une leçon</p>
            </body></html>
        "#;
        let text = render(html);
        assert_eq!(text, "Commence une leçon", "got {text:?}");
    }

    /// A body made of thousands of empty skipped tags used to cost time
    /// quadratic in the document size, because each skip lowercased the
    /// whole remaining document. The skip is allocation-free now; this
    /// pins the behaviour (the text around the tags still renders) rather
    /// than a wall-clock bound, which would be flaky on CI.
    #[test]
    fn survives_thousands_of_empty_skipped_tags() {
        let mut html = String::from("<p>Début</p>");
        for _ in 0..5_000 {
            html.push_str("<style></style>");
        }
        html.push_str("<p>Fin</p>");
        let text = render(&html);
        assert_eq!(text, "Début\nFin", "got {text:?}");
    }

    #[test]
    fn closing_tag_search_is_case_insensitive_and_prefix_exact() {
        assert_eq!(find_closing_tag("x</STYLE>y", "style"), Some(1));
        assert_eq!(find_closing_tag("x</style>y", "style"), Some(1));
        // `</stylex` must not count as the closing tag for `style`.
        assert_eq!(find_closing_tag("</stylex>", "style"), None);
        assert_eq!(find_closing_tag("</script>", "style"), None);
        assert_eq!(find_closing_tag("no tag here", "style"), None);
    }

    #[test]
    fn keeps_bold_italic_and_links_as_markup() {
        let html = r#"<p><b>Tu fais de gros progrès !</b> <a href="https://ex.com/a?b=1&amp;c=2">Commence</a> <i>doucement</i></p>"#;
        let spans = spans_from_html(html);
        let out = html_of(&spans);
        assert!(out.contains("<b>Tu fais de gros progrès !</b>"), "{out:?}");
        assert!(
            out.contains(r#"<a href="https://ex.com/a?b=1&amp;c=2">Commence</a>"#),
            "{out:?}"
        );
        assert!(out.contains("<i>doucement</i>"), "{out:?}");
    }

    #[test]
    fn flattens_a_streak_table_to_one_line_per_row() {
        let html = r#"
            <table role="presentation">
              <tr><td>V<br>:</td><td>✅</td><td>S<br>:</td><td>💾</td></tr>
              <tr><td>D<br>:</td><td>✅</td><td>L<br>:</td><td>💾</td></tr>
            </table>
        "#;
        assert_eq!(render(html), "V : ✅ S : 💾\nD : ✅ L : 💾");
    }

    #[test]
    fn decodes_entities_and_strips_invisible_padding() {
        assert_eq!(
            render("<p>caf&eacute; &#233; &#xE9; &amp; &lt;ok&gt;</p>"),
            "café é é & <ok>"
        );
        assert_eq!(render("<p>a\u{200b}b&#847;c</p>"), "abc");
    }

    #[test]
    fn keeps_emoji_joins_and_variation_selectors_intact() {
        // Stripping ZWJ or U+FE0F would turn one emoji into several glyphs.
        assert_eq!(
            render("<p>👨\u{200d}👩\u{200d}👧 ❤\u{fe0f}</p>"),
            "👨\u{200d}👩\u{200d}👧 ❤\u{fe0f}"
        );
    }

    #[test]
    fn ignores_scripts_and_images_but_keeps_an_image_button_label() {
        assert_eq!(render("<script>alert(1)</script><p>ok</p>"), "ok");
        let out = html_of(&spans_from_html(
            r#"<a href="https://ex.com/go"><img src="button.png" alt="Commencer"></a>"#,
        ));
        assert!(
            out.contains("<a href=\"https://ex.com/go\">Commencer</a>"),
            "{out:?}"
        );
    }

    #[test]
    fn an_image_only_link_without_alt_stays_reachable() {
        let out = html_of(&spans_from_html(
            r#"<a href="https://ex.com/go"><img src="x.png"></a>"#,
        ));
        assert_eq!(out, r#"<a href="https://ex.com/go">https://ex.com/go</a>"#);
    }

    #[test]
    fn rejects_dangerous_link_schemes() {
        let out = html_of(&spans_from_html(
            r#"<a href="javascript:alert(1)">x</a><a href="data:text/html,hi">y</a>"#,
        ));
        assert_eq!(out, "xy");
    }

    #[test]
    fn collapses_layout_newlines_to_a_single_break() {
        let html = "<div>un</div><div></div><div><br></div><p><br></p><div>deux</div>";
        assert_eq!(render(html), "un\ndeux");
    }

    #[test]
    fn chunking_stays_under_the_limit_and_reopens_tags() {
        let spans = spans_from_html("<p><b>aaaa bbbb cccc dddd</b></p>");
        let chunks = chunk_spans(&spans, 10);
        assert!(chunks.len() > 1, "{chunks:?}");
        for chunk in &chunks {
            let visible: usize = chunk
                .replace("<b>", "")
                .replace("</b>", "")
                .encode_utf16()
                .count();
            assert!(visible <= 10, "chunk over budget: {chunk:?}");
            assert_eq!(chunk.matches("<b>").count(), chunk.matches("</b>").count());
        }
        assert!(chunks[1].starts_with("<b>"), "{chunks:?}");
    }

    #[test]
    fn chunking_counts_utf16_units_not_chars() {
        // Each emoji is two UTF-16 units, so only two fit in a budget of 4.
        let spans = spans_from_html("<p>😀😀😀</p>");
        let chunks = chunk_spans(&spans, 4);
        assert_eq!(chunks, vec!["😀😀".to_string(), "😀".to_string()]);
    }

    #[test]
    fn chunking_a_link_keeps_each_chunk_valid() {
        let spans = spans_from_html(
            r#"<a href="https://ex.com/very/long/target">un deux trois quatre cinq</a>"#,
        );
        let chunks = chunk_spans(&spans, 8);
        for chunk in &chunks {
            assert_eq!(chunk.matches("<a ").count(), chunk.matches("</a>").count());
        }
    }

    #[test]
    fn a_duolingo_shaped_body_reads_like_a_message() {
        let html = r#"
            <div style="display:none;max-height:0">Préheader</div>
            <img src="https://cdn.duolingo.com/logo.png" alt="Duolingo">
            <h1 style="text-transform:uppercase">Commence une leçon</h1>
            <a href="https://www.duolingo.com/course/ja/fr?email_type=practice_reminder">COMMENCE UNE LEÇON</a>
            <p><b>Tu fais de gros progrès !</b><br>Continue comme ça, DeltaWhiplash.</p>
            <table><tr><td>V<br>:</td><td>✅</td><td>S<br>:</td><td>💾</td></tr></table>
            <hr>
            <p>Si tu veux arrêter les rappels, <a href="https://www.duolingo.com/unsubscribe">clique ici</a>.</p>
        "#;
        let text = render(html);
        assert!(text.starts_with("Commence une leçon"), "{text:?}");
        assert!(text.contains("COMMENCE UNE LEÇON"), "{text:?}");
        assert!(text.contains("Tu fais de gros progrès !"), "{text:?}");
        assert!(!text.contains("**"), "literal markdown leaked: {text:?}");
        assert!(!text.contains("Préheader"), "preheader leaked: {text:?}");
        assert!(
            !text.contains("duolingo.com/course"),
            "raw URL leaked: {text:?}"
        );
        assert!(text.contains("V : ✅ S : 💾"), "{text:?}");
        assert!(!text.contains("----"), "separator leaked: {text:?}");

        let out = html_of(&spans_from_html(html));
        assert!(out.contains(
            r#"href="https://www.duolingo.com/course/ja/fr?email_type=practice_reminder""#
        ));
        assert!(out.contains("<b>Tu fais de gros progrès !</b>"), "{out:?}");
    }
}
