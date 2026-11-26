//! Test-only enforcement of the parts of Telegram's message contract that
//! this bot can actually violate.
//!
//! The mock Telegram server in `src/bot.rs` used to answer `ok` to every
//! `sendMessage`, so a payload real Telegram rejects - an unescaped `&`, an
//! unbalanced `<b>`, a message over 4096 UTF-16 units - passed the whole
//! suite and only failed in production, where the failure surfaces as a
//! missing message rather than a test. The mock now feeds every outbound
//! text through [`violation`] and answers 400 the way Telegram does.
//!
//! The checks are deliberately narrow: they cover what the bot itself
//! produces (HTML markup from `render.rs`, MarkdownV2 from `format.rs`) and
//! nothing Telegram accepts but we never emit. A false positive here fails
//! a test that would have passed in production, so anything unmodelled is
//! allowed rather than guessed at.

/// Telegram's hard limit, counted in UTF-16 code units *after* entity
/// parsing - the same unit the chunkers budget against.
pub const MAX_MESSAGE_UNITS: usize = 4096;

/// Tags Telegram documents for `parse_mode=HTML`. A tag outside this list
/// is shown literally by Telegram, which means the message arrives with
/// markup the reader can see - a rendering bug, not a delivery failure.
const ALLOWED_TAGS: [&str; 15] = [
    "b",
    "strong",
    "i",
    "em",
    "u",
    "ins",
    "s",
    "strike",
    "del",
    "a",
    "code",
    "pre",
    "blockquote",
    "tg-spoiler",
    "span",
];

/// Returns why Telegram would reject this request, or `None` when it would
/// accept it.
pub fn violation(text: &str, parse_mode: Option<&str>) -> Option<String> {
    match parse_mode {
        Some(mode) if mode.eq_ignore_ascii_case("html") => html_violation(text),
        // MarkdownV2 and the no-parse-mode case get the length check only:
        // validating MarkdownV2 escapes faithfully would mean reimplementing
        // Telegram's parser, and a wrong rule there fails tests that pass in
        // production.
        _ => length_violation(visible_units(text)),
    }
}

fn length_violation(units: usize) -> Option<String> {
    (units > MAX_MESSAGE_UNITS).then(|| {
        format!("message is {units} UTF-16 units, Telegram's limit is {MAX_MESSAGE_UNITS}")
    })
}

/// UTF-16 length of the text Telegram ends up delivering.
fn visible_units(text: &str) -> usize {
    text.encode_utf16().count()
}

fn html_violation(text: &str) -> Option<String> {
    let mut stack: Vec<&str> = Vec::new();
    let mut visible = String::with_capacity(text.len());
    let mut i = 0;

    while i < text.len() {
        if text.as_bytes()[i] == b'<' {
            let rest = &text[i..];
            let end = match rest.find('>') {
                Some(end) => end,
                None => return Some(format!("unterminated tag at byte {i}")),
            };
            let tag = &rest[1..end];
            if let Some(name) = tag.strip_prefix('/') {
                let name = name.trim();
                match stack.pop() {
                    Some(open) if open == name => {}
                    Some(open) => {
                        return Some(format!("</{name}> closes <{open}> out of order"));
                    }
                    None => return Some(format!("</{name}> closes a tag that is not open")),
                }
            } else {
                let (name, attributes) = split_tag(tag);
                if !ALLOWED_TAGS.contains(&name) {
                    return Some(format!(
                        "<{name}> is not a tag Telegram supports in HTML mode"
                    ));
                }
                if let Some(reason) = attribute_violation(name, attributes) {
                    return Some(reason);
                }
                stack.push(name);
            }
            i += end + 1;
        } else if text.as_bytes()[i] == b'&' {
            match entity(&text[i..]) {
                // Entities are decoded before Telegram counts: `&amp;` is
                // one delivered character, `&#x1F600;` is two UTF-16 units.
                Some((len, decoded)) => {
                    visible.push(decoded);
                    i += len;
                }
                None => {
                    return Some(format!(
                        "unescaped '&' at byte {i}: Telegram needs &amp;, &lt;, &gt; or &quot;"
                    ));
                }
            }
        } else {
            // Copy one UTF-8 character, never splitting a boundary.
            let ch = text[i..].chars().next().expect("i is a char boundary");
            visible.push(ch);
            i += ch.len_utf8();
        }
    }

    if let Some(open) = stack.last() {
        return Some(format!("<{open}> is never closed"));
    }

    length_violation(visible.encode_utf16().count())
}

/// `b` / `b class="x"` -> (`b`, `class="x"`).
fn split_tag(tag: &str) -> (&str, &str) {
    let name_end = tag.find(|c: char| c.is_whitespace()).unwrap_or(tag.len());
    let (name, rest) = tag.split_at(name_end);
    (name, rest.trim())
}

fn attribute_violation(name: &str, attributes: &str) -> Option<String> {
    match name {
        // `<a href="...">` carries a link; an anchor without one is not
        // tappable, which is the whole point of the tag.
        "a" => {
            let value = match attribute_value(attributes, "href") {
                Some(value) => value,
                None => return Some("<a> without an href attribute".to_string()),
            };
            if value.is_empty() {
                return Some("<a href=\"\"> would not be tappable".to_string());
            }
            None
        }
        // `<span class="tg-spoiler">` is Telegram's only span form.
        "span" => match attribute_value(attributes, "class") {
            Some("tg-spoiler") => None,
            _ => Some("<span> is only supported with class=\"tg-spoiler\"".to_string()),
        },
        "code" | "pre" if attributes.is_empty() => None,
        // `<pre><code class="language-rust">` is the documented form.
        "code" | "pre" => match attribute_value(attributes, "class") {
            Some(value) if value.starts_with("language-") => None,
            _ => Some(format!("<{name}> only accepts class=\"language-...\"")),
        },
        _ if attributes.is_empty() => None,
        _ => Some(format!("<{name}> does not take attributes")),
    }
}

/// Extracts `name="value"` (single or double quoted) from an attribute list.
fn attribute_value<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=");
    let start = attributes.find(&needle)? + needle.len();
    let rest = &attributes[start..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let body = &rest[1..];
    let end = body.find(quote)?;
    Some(&body[..end])
}

/// The entity starting at `text` (which begins with `&`): its length in
/// bytes and the character it decodes to. `None` when Telegram would reject
/// it as a bare ampersand.
fn entity(text: &str) -> Option<(usize, char)> {
    let rest = text.strip_prefix('&')?;
    let end = rest.find(';')?;
    let name = &rest[..end];
    let decoded = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        _ => {
            let hex = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X"));
            let code = match hex {
                Some(hex) if !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()) => {
                    u32::from_str_radix(hex, 16).ok()?
                }
                _ => {
                    let dec = name.strip_prefix('#')?;
                    if dec.is_empty() || !dec.chars().all(|c| c.is_ascii_digit()) {
                        return None;
                    }
                    dec.parse().ok()?
                }
            };
            char::from_u32(code)?
        }
    };
    Some((end + 2, decoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(text: &str) -> Option<String> {
        violation(text, Some("HTML"))
    }

    #[test]
    fn accepts_the_rich_body_shape_the_read_all_button_sends() {
        assert_eq!(
            html(
                "<b>Ta série</b>\n<a href=\"https://duolingo.com/x\">Commence une leçon &amp; plus</a>"
            ),
            None
        );
    }

    #[test]
    fn rejects_an_unescaped_ampersand() {
        let violation = html("Tom & Jerry").expect("a bare & is a 400 on Telegram");
        assert!(violation.contains("unescaped '&'"), "{violation}");
        assert_eq!(html("Tom &amp; Jerry"), None);
        assert_eq!(html("Tom &#38; Jerry"), None);
    }

    #[test]
    fn rejects_unbalanced_or_unknown_markup() {
        assert!(html("<b>open").expect("unclosed").contains("never closed"));
        assert!(html("</b>").expect("stray close").contains("not open"));
        assert!(
            html("<b><i>x</b></i>")
                .expect("mismatched")
                .contains("out of order")
        );
        assert!(html("<div>x</div>").expect("unknown").contains("not a tag"));
    }

    #[test]
    fn rejects_links_without_a_usable_href() {
        assert!(
            html("<a>x</a>")
                .expect("missing href")
                .contains("without an href")
        );
        assert!(
            html("<a href=\"\">x</a>")
                .expect("empty href")
                .contains("not be tappable")
        );
        assert_eq!(html("<a href=\"https://example.org\">x</a>"), None);
    }

    #[test]
    fn rejects_the_spoiler_span_shape_telegram_does_not_know() {
        assert!(
            html("<span>x</span>")
                .expect("no class")
                .contains("tg-spoiler")
        );
        assert_eq!(html("<span class=\"tg-spoiler\">x</span>"), None);
    }

    #[test]
    fn counts_utf16_units_the_way_telegram_does() {
        assert_eq!(html(&"a".repeat(MAX_MESSAGE_UNITS)), None);
        assert!(
            html(&"a".repeat(MAX_MESSAGE_UNITS + 1))
                .expect("one unit too long")
                .contains("limit")
        );

        // An emoji is one `char` but two UTF-16 units: 2048 of them fit,
        // 2049 do not - the exact off-by-a-factor a `chars().count()`
        // budget would miss.
        assert_eq!(html(&"😀".repeat(MAX_MESSAGE_UNITS / 2)), None);
        assert!(html(&"😀".repeat(MAX_MESSAGE_UNITS / 2 + 1)).is_some());
    }

    /// A Duolingo-shaped "read all" body: hidden preheader, tracked links,
    /// an emoji streak table, entities and a footer link. This is the class
    /// of message the old text/plain forwarding mangled, so it doubles as
    /// the end-to-end guard: whatever `render.rs` produces for it must be
    /// acceptable to the mock now standing in for Telegram.
    fn duolingo_body() -> &'static str {
        r#"<html><head><style>a > b { color: red }</style></head><body>
        <div style="display:none;max-height:0">Aperçu caché du message</div>
        <a href="https://www.duolingo.com/"><img alt="Duolingo" src="logo.png"></a>
        <h1>Chaque leçon compte</h1>
        <p><b>Tu fais de gros progrès !</b> Tom &amp; Jerry &lt;3 &quot;bravo&quot;</p>
        <table><tr><td>V</td><td>:</td><td>✅</td><td>S</td><td>:</td><td>💾</td></tr>
        <tr><td>D</td><td>:</td><td>✅</td><td>L</td><td>:</td><td>💾</td></tr></table>
        <p><a href="https://www.duolingo.com/course/ja/fr?email_type=practice_reminder&amp;utm_campaign=streak">COMMENCE UNE LEÇON</a></p>
        <p>Si tu veux arrêter de recevoir ces rappels,
        <a href="https://www.duolingo.com/unsubscribe?u=abc&amp;t=xyz">clique ici</a>.</p>
        </body></html>"#
    }

    #[test]
    fn the_rendered_read_all_body_satisfies_the_contract() {
        let spans = crate::render::spans_from_html(duolingo_body());
        let chunks =
            crate::render::chunk_spans(&spans, crate::format::TELEGRAM_MAX_MESSAGE_LEN - 16);

        assert!(!chunks.is_empty(), "the body must render to something");
        for chunk in &chunks {
            assert_eq!(
                violation(chunk, Some("HTML")),
                None,
                "Telegram would reject this chunk:\n{chunk}"
            );
        }

        let joined = chunks.join("\n");
        // Assert on what the reader sees, not on where the renderer puts a
        // space: a style boundary used to swallow it and glue the runs.
        assert!(
            visible(&joined).contains("Tu fais de gros progrès ! Tom & Jerry <3 \"bravo\""),
            "{joined}"
        );
        // `"` is only special inside an attribute, so it reaches Telegram raw.
        assert!(
            joined.contains("Tom &amp; Jerry &lt;3 \"bravo\""),
            "{joined}"
        );
        assert!(joined.contains("V : ✅ S : 💾"), "{joined}");
        // The tracked URL must reach Telegram escaped, inside the href.
        assert!(
            joined.contains("email_type=practice_reminder&amp;utm_campaign=streak"),
            "{joined}"
        );
    }

    /// What Telegram shows after parsing: tags removed, entities decoded.
    fn visible(html: &str) -> String {
        let mut out = String::new();
        let mut in_tag = false;
        for ch in html.chars() {
            match ch {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => out.push(ch),
                _ => {}
            }
        }
        out.replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
    }

    #[test]
    fn a_body_that_must_be_split_stays_valid_in_every_chunk() {
        // One unbreakable word forces `chunk_spans` to split mid-link, which
        // is where closed-then-reopened <b>/<i>/<a> markup used to go wrong.
        let long = "x".repeat(crate::format::TELEGRAM_MAX_MESSAGE_LEN);
        let html = format!(
            "<p><b>bold</b> <a href=\"https://duolingo.com/x?a=1&amp;b=2\">{long}</a></p>             <p>😀 emoji 😀 après &amp; après</p>"
        );
        let spans = crate::render::spans_from_html(&html);
        let chunks =
            crate::render::chunk_spans(&spans, crate::format::TELEGRAM_MAX_MESSAGE_LEN - 16);

        assert!(chunks.len() > 1, "the fixture must actually split");
        for (index, chunk) in chunks.iter().enumerate() {
            assert_eq!(
                violation(chunk, Some("HTML")),
                None,
                "chunk {index} would be rejected:\n{chunk}"
            );
        }
    }

    #[test]
    fn plain_and_markdown_text_is_only_checked_for_length() {
        assert_eq!(violation("<b>literal</b>", None), None);
        assert_eq!(violation("*not markup here*", Some("MarkdownV2")), None);
        assert!(violation(&"a".repeat(MAX_MESSAGE_UNITS + 1), Some("MarkdownV2")).is_some());
    }

    #[test]
    fn entities_do_not_inflate_the_length_telegram_counts() {
        // 4096 delivered characters, each written as a 5-byte entity: the
        // raw text is far longer, the delivered message is exactly at the
        // limit.
        let text = "&amp;".repeat(MAX_MESSAGE_UNITS);
        assert_eq!(html(&text), None);
    }
}
