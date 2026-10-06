//! Mail HTML sanitisation — the step `design/08` §4.3 and `design/14` §5.1 both mandate.
//!
//! **Why this module exists.** The renderer wraps the message body in a sandboxed `iframe` with a
//! `default-src 'none'` CSP, and for a long time that was the whole defence: the body was inserted
//! into the sandbox document raw. An audit on 2026-10-06 found that this leaves two holes the CSP
//! cannot close:
//!
//! * `<meta http-equiv="refresh" content="0;url=https://tracker.example/px">` fires an unsolicited
//!   `GET` when the message is opened. `default-src` does not cover navigations, and a sandboxed
//!   frame is always allowed to navigate **itself** — `allow-top-navigation` only protects the top
//!   frame. That is a tracking pixel that ignores the "显示图片" opt-in entirely.
//! * `<iframe srcdoc>`, `<object>`, `<embed>` and `on*=` handlers are a standing invitation to
//!   whatever the CSP does not enumerate.
//!
//! So the HTML is now stripped **before** it is stored and **again** when it is read back (rows
//! cached before this module existed still hold the raw text).
//!
//! **What this is not.** It is not a general HTML sanitiser with an allow-list of elements and
//! attributes; writing one by hand is a known way to lose. It removes a defined, testable set of
//! dangerous constructs, conservatively: where the parse is ambiguous it drops rather than keeps.
//! The sandbox and the CSP remain the primary defence — this is the layer that has to hold when the
//! CSP has no opinion, and the tests below are the statement of what it does.

/// Elements removed **with their content**.
///
/// Content matters: leaving `<script>` out while keeping `alert(1)` would render the source code of
/// the script as visible text, and some of these elements (`xmp`, `plaintext`, `noscript`) change
/// how everything after them is parsed.
///
/// `embed` is deliberately **not** here: it is a void element, so searching for `</embed>` would eat
/// the rest of the message. See [`DROP_TAG`].
const DROP_ELEMENT: [&str; 14] = [
    "script",
    "iframe",
    "frameset",
    "object",
    "applet",
    "noscript",
    "template",
    "xmp",
    "plaintext",
    "noembed",
    "noframes",
    "title",
    "textarea",
    "foreignobject",
];

/// Void or attribute-only elements, removed without looking for a closing tag.
///
/// `meta` is the important one: `http-equiv="refresh"` is the navigation hole described above, and
/// `http-equiv="Content-Security-Policy"` inside the body could *widen* the policy the renderer set.
/// `embed` and `frame` are void in HTML, so consuming content after them would delete real mail.
const DROP_TAG: [&str; 6] = ["meta", "base", "link", "frame", "embed", "basefont"];

/// Attributes removed by name wherever they appear.
///
/// `srcdoc` is a whole document in an attribute; `formaction` overrides a form's target on the
/// button itself, so filtering `<form>` alone would not be enough.
const DROP_ATTR: [&str; 6] = [
    "srcdoc",
    "formaction",
    "background",
    "dynsrc",
    "lowsrc",
    "action",
];

/// Sanitise a message body for rendering inside the sandbox.
///
/// Idempotent: running it over its own output changes nothing, which is what makes it safe to call
/// both when a message is parsed and when it is read back out of the cache.
pub fn sanitize_mail_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < input.len() {
        let rest = &input[i..];
        let Some(lt) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..lt]);
        i += lt;

        match parse_tag(&input[i..]) {
            // A `<` that starts nothing is text, and it must be re-escaped rather than emitted: a
            // bare `<` left in the output is exactly how a tag gets reassembled across two
            // sanitised fragments.
            Piece::Lone => {
                out.push_str("&lt;");
                i += 1;
            }
            // A comment, doctype or processing instruction: dropped whole. Dropping it byte by byte
            // would leave its interior as visible text, and a conditional comment is a classic place
            // to hide a `<script>` from a scanner that only looks at tag boundaries.
            Piece::Skip(len) => i += len,
            Piece::Tag(tag) => {
                let name = tag.name.as_str();
                let after = i + tag.len;

                // An unterminated tag takes the rest of the input with it. Re-emitting `<scr` and
                // letting the following text complete it would hand the renderer the very tag this
                // function exists to remove.
                if tag.unterminated {
                    break;
                }
                if DROP_ELEMENT.contains(&name) {
                    i = skip_element(input, after, name);
                    continue;
                }
                if DROP_TAG.contains(&name) {
                    i = after;
                    continue;
                }

                // A closing tag has no attributes worth keeping, and `<style>`/`<svg>` content is
                // left alone (CSS cannot execute script in the engines this app supports, and
                // `style-src 'unsafe-inline'` is deliberate: mail layout depends on it).
                out.push_str(&rebuild_tag(&input[i..i + tag.len], &tag));
                i = after;
            }
        }
    }

    out
}

/// Remove bidirectional formatting characters and other invisible controls from a string.
///
/// Shared with the filename sanitiser in the desktop crate. These characters are how `exe.txt`
/// becomes something that *displays* as `txt.exe`: the defence is to delete them, not to escape
/// them, because there is no legitimate reason for a filename or a URL to contain one.
pub fn strip_invisible_controls(input: &str) -> String {
    input
        .chars()
        .filter(|c| !is_invisible_control(*c))
        .collect()
}

fn is_invisible_control(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}'   // zero-width space … RLM
        | '\u{202A}'..='\u{202E}' // LRE … RLO (the classic filename spoof)
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}' // LRI … PDI
        | '\u{FEFF}'              // BOM / zero-width no-break
        | '\u{00AD}'              // soft hyphen
    )
}

// --------------------------------------------------------------------------------------- parsing

struct Tag {
    /// Lower-cased element name.
    name: String,
    /// Length of the tag in bytes, including its angle brackets.
    len: usize,
    attrs: Vec<Attr>,
    /// The tag ran to the end of the input without a `>`.
    unterminated: bool,
}

enum Piece {
    /// A real tag to be filtered.
    Tag(Tag),
    /// Bytes to drop whole (comment, doctype, processing instruction).
    Skip(usize),
    /// A `<` that does not start anything.
    Lone,
}

struct Attr {
    /// Byte range of `name[=value]` inside the tag, excluding surrounding whitespace.
    start: usize,
    end: usize,
    name: String,
    /// Decoded value; empty when the attribute has none (`<input disabled>`).
    value: String,
    has_value: bool,
}

/// Parse one tag starting at, and including, `<`.
fn parse_tag(src: &str) -> Piece {
    debug_assert!(src.starts_with('<'));
    let bytes = src.as_bytes();

    let Some(&second) = bytes.get(1) else {
        return Piece::Lone;
    };

    // Comments, doctypes and processing instructions are dropped whole rather than character by
    // character: a conditional comment (`<!--[if IE]><script>…`) is precisely a construct built to
    // hide a tag from a scanner that only looks at tag boundaries.
    if second == b'!' {
        if src[1..].starts_with("!--") {
            return match src.find("-->") {
                Some(end) => Piece::Skip(end + 3),
                None => Piece::Skip(src.len()),
            };
        }
        return match src.find('>') {
            Some(end) => Piece::Skip(end + 1),
            None => Piece::Skip(src.len()),
        };
    }
    if second == b'?' {
        return match src.find('>') {
            Some(end) => Piece::Skip(end + 1),
            None => Piece::Skip(src.len()),
        };
    }
    if !second.is_ascii_alphabetic() && second != b'/' {
        return Piece::Lone;
    }

    let mut i = 1;
    let closing = second == b'/';
    if closing {
        i += 1;
    }

    let name_start = i;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b':')
    {
        i += 1;
    }
    if i == name_start {
        return Piece::Lone;
    }
    let name = src[name_start..i].to_ascii_lowercase();

    let mut attrs = Vec::new();
    // Scan to the closing `>` while collecting attributes. A quoted `>` does not end the tag —
    // getting that wrong is how `<a title="a>b" href="javascript:...">` slips through.
    let mut quote: Option<u8> = None;
    loop {
        if i >= bytes.len() {
            // Unterminated: the caller drops the rest rather than re-emitting a partial tag that the
            // following text could complete.
            return Piece::Tag(Tag {
                name,
                len: src.len(),
                attrs: Vec::new(),
                unterminated: true,
            });
        }
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => {
                quote = Some(b);
                i += 1;
            }
            b'>' => {
                i += 1;
                break;
            }
            _ => {
                // Attribute name.
                let start = i;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && bytes[i] != b'='
                    && bytes[i] != b'>'
                    && bytes[i] != b'/'
                {
                    i += 1;
                }
                if i == start {
                    // A stray `/` (self-closing) or whitespace.
                    i += 1;
                    continue;
                }
                let attr_name = src[start..i].to_ascii_lowercase();

                let mut value = String::new();
                let mut has_value = false;
                let mut end = i;
                let mut probe = i;
                while probe < bytes.len() && bytes[probe].is_ascii_whitespace() {
                    probe += 1;
                }
                if probe < bytes.len() && bytes[probe] == b'=' {
                    probe += 1;
                    while probe < bytes.len() && bytes[probe].is_ascii_whitespace() {
                        probe += 1;
                    }
                    has_value = true;
                    if probe < bytes.len() && (bytes[probe] == b'"' || bytes[probe] == b'\'') {
                        let q = bytes[probe];
                        let vstart = probe + 1;
                        let mut vend = vstart;
                        while vend < bytes.len() && bytes[vend] != q {
                            vend += 1;
                        }
                        value = decode_entities(&src[vstart..vend.min(bytes.len())]);
                        i = (vend + 1).min(bytes.len());
                    } else {
                        let vstart = probe;
                        let mut vend = vstart;
                        while vend < bytes.len()
                            && !bytes[vend].is_ascii_whitespace()
                            && bytes[vend] != b'>'
                        {
                            vend += 1;
                        }
                        value = decode_entities(&src[vstart..vend]);
                        i = vend;
                    }
                    end = i;
                }

                attrs.push(Attr {
                    start,
                    end,
                    name: attr_name,
                    value,
                    has_value,
                });
            }
        }
    }

    Piece::Tag(Tag {
        name,
        len: i,
        attrs,
        unterminated: false,
    })
}

/// Emit the tag with the dangerous attributes cut out.
///
/// The kept attributes are copied verbatim from the input rather than re-serialised, so quoting and
/// spacing survive exactly as the sender wrote them and this function cannot introduce a new
/// quoting bug of its own.
fn rebuild_tag(original: &str, tag: &Tag) -> String {
    let mut kept = String::with_capacity(original.len());
    let mut cursor = 0;

    for attr in &tag.attrs {
        if drop_attribute(attr) {
            // Also swallow the whitespace immediately before the attribute.
            let mut cut = attr.start;
            while cut > cursor && original.as_bytes()[cut - 1].is_ascii_whitespace() {
                cut -= 1;
            }
            kept.push_str(&original[cursor..cut]);
            cursor = attr.end;
        }
    }
    kept.push_str(&original[cursor..]);

    kept
}

fn drop_attribute(attr: &Attr) -> bool {
    if attr.name.starts_with("on") {
        return true;
    }
    if DROP_ATTR.contains(&attr.name.as_str()) {
        return true;
    }
    // `style` is kept — mail layout needs it — but the two constructs that could turn it into code
    // in the engines this app can encounter are cut.
    if attr.name == "style" && attr.has_value {
        let v = attr.value.to_ascii_lowercase();
        if v.contains("expression(") || v.contains("javascript:") || v.contains("vbscript:") {
            return true;
        }
    }
    if attr.has_value && is_url_attribute(&attr.name) {
        return is_dangerous_url(&attr.value);
    }
    false
}

fn is_url_attribute(name: &str) -> bool {
    matches!(
        name,
        "href" | "src" | "xlink:href" | "poster" | "data" | "cite" | "longdesc" | "usemap"
    )
}

/// Is this URL something the reading pane must never be handed?
///
/// The comparison is deliberately *lossy* in the safe direction: entities are decoded, then every
/// whitespace, control and bidi character is removed, then the prefix is compared. `java\tscript:`
/// and `&#106;avascript:` both become `javascript:`; a legitimate `javascript-guide.html` does not
/// match, because the colon is still required.
pub fn is_dangerous_url(raw: &str) -> bool {
    let decoded = decode_entities(raw);
    let cleaned: String = strip_invisible_controls(&decoded)
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect::<String>()
        .to_ascii_lowercase();

    if cleaned.starts_with("javascript:")
        || cleaned.starts_with("vbscript:")
        || cleaned.starts_with("file:")
        || cleaned.starts_with("ms-")
        || cleaned.starts_with("mhtml:")
        || cleaned.starts_with("jar:")
    {
        return true;
    }

    // `data:` is allowed for images only: that is how `cid:` parts are handed to the renderer, and
    // there is no other reason for a mail body to carry an inline payload.
    if cleaned.starts_with("data:") {
        return !cleaned.starts_with("data:image/");
    }

    false
}

/// Skip past an element's content, up to and including its closing tag.
///
/// When there is no closing tag the rest of the document goes with it. That is the conservative
/// direction on purpose: `<script>` without `</script>` is malformed, and guessing where it ends is
/// how the classic `<script>`-inside-a-`<script>` bypass works.
fn skip_element(input: &str, from: usize, name: &str) -> usize {
    let needle = format!("</{name}");
    let haystack = &input[from..];
    let lower = haystack.to_ascii_lowercase();

    match lower.find(&needle) {
        Some(pos) => {
            let after_name = from + pos + needle.len();
            match input[after_name..].find('>') {
                Some(close) => after_name + close + 1,
                None => input.len(),
            }
        }
        None => input.len(),
    }
}

// ------------------------------------------------------------------------------------- entities

/// Decode the entities that can appear inside a scheme.
///
/// Not the full HTML5 table: a named entity can only matter here if it produces a character that
/// could be part of `javascript:` or `data:`, and the set of named entities for letters and digits
/// is empty. Numeric forms are handled generally, because `&#106;` is a letter.
fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        // A ";" more than 12 bytes away is not an entity.
        if semi > 12 {
            out.push_str(&after[..1]);
            rest = &after[1..];
            continue;
        }
        let body = &after[1..semi];
        let decoded = if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X"))
        {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
        } else if let Some(dec) = body.strip_prefix('#') {
            dec.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            match body.to_ascii_lowercase().as_str() {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "39" => Some('\''),
                "colon" => Some(':'),
                "sol" => Some('/'),
                "period" => Some('.'),
                "tab" => Some('\t'),
                "newline" => Some('\n'),
                "nbsp" => Some('\u{00A0}'),
                _ => None,
            }
        };

        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The security-critical table. Every case here is a construct a mail body can carry and the
    /// reading pane must not act on; the fixture list comes from `design/14` §9.
    #[test]
    fn the_dangerous_constructs_are_all_removed() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "meta refresh is the navigation hole the CSP cannot close",
                r#"<meta http-equiv="refresh" content="0;url=https://tracker.example/px">hello"#,
                "hello",
            ),
            (
                "script with content",
                "<p>a</p><script>alert(document.cookie)</script><p>b</p>",
                "<p>a</p><p>b</p>",
            ),
            (
                "script whose closing tag is missing takes the rest with it",
                "<p>a</p><script>alert(1)",
                "<p>a</p>",
            ),
            (
                "inline event handler",
                r#"<img src="cid:x" onerror="alert(1)">"#,
                r#"<img src="cid:x">"#,
            ),
            (
                "javascript: in a link",
                r#"<a href="javascript:alert(1)">x</a>"#,
                "<a>x</a>",
            ),
            (
                "javascript: hidden behind an entity",
                r#"<a href="&#106;avascript:alert(1)">x</a>"#,
                "<a>x</a>",
            ),
            (
                "javascript: split by a tab",
                "<a href=\"java\tscript:alert(1)\">x</a>",
                "<a>x</a>",
            ),
            (
                "javascript: split by a bidi override",
                "<a href=\"java\u{202E}script:alert(1)\">x</a>",
                "<a>x</a>",
            ),
            (
                "an iframe pointing anywhere",
                r#"<iframe src="https://evil.example"></iframe>ok"#,
                "ok",
            ),
            (
                "srcdoc is a whole document in an attribute",
                r#"<iframe srcdoc="<script>x</script>">"#,
                "",
            ),
            (
                "object and embed",
                r#"<object data="x.swf"></object><embed src="y.swf">z"#,
                "z",
            ),
            (
                "base rewrites every relative URL in the document",
                r#"<base href="https://evil.example/">"#,
                "",
            ),
            (
                "a data: URL that is not an image",
                r#"<a href="data:text/html,<script>alert(1)</script>">x</a>"#,
                "<a>x</a>",
            ),
            (
                "formaction on a button",
                r#"<form><button formaction="javascript:alert(1)">go</button></form>"#,
                "<form><button>go</button></form>",
            ),
            (
                "an entity disguised as a tag",
                "&lt;script&gt;alert(1)&lt;/script&gt;",
                "&lt;script&gt;alert(1)&lt;/script&gt;",
            ),
            (
                "a tag split by a newline",
                "<img src=x\nonerror=alert(1)>",
                "<img src=x>",
            ),
            (
                "a quoted angle bracket does not end the tag early",
                r#"<a title="a>b" href="javascript:alert(1)">x</a>"#,
                r#"<a title="a>b">x</a>"#,
            ),
            (
                "an unconditional comment can hide a conditional script",
                "<!--[if IE]><script>alert(1)</script><![endif]--><p>keep</p>",
                "<p>keep</p>",
            ),
            (
                "a style expression is old IE script execution",
                r#"<div style="width: expression(alert(1))">x</div>"#,
                "<div>x</div>",
            ),
        ];

        for (why, input, expected) in cases {
            assert_eq!(&sanitize_mail_html(input), expected, "case: {why}");
        }
    }

    /// What must survive: ordinary mail is mostly tables, inline styles and `cid:` images, and a
    /// sanitiser that breaks those is a sanitiser nobody can ship.
    #[test]
    fn ordinary_mail_is_left_alone() {
        let keep = [
            r#"<p style="color:red">你好</p>"#,
            r#"<table width="100%"><tr><td style="width:720px">cell</td></tr></table>"#,
            r#"<img src="data:image/png;base64,iVBORw0KGgo=" width="16" height="16">"#,
            r#"<a href="https://example.com/a?b=c&amp;d=e">link</a>"#,
            r#"<blockquote>quoted</blockquote>"#,
            r#"<span class="x" data-ee-href="https://example.com">y</span>"#,
        ];
        for html in keep {
            assert_eq!(sanitize_mail_html(html), html, "must be untouched: {html}");
        }
    }

    #[test]
    fn sanitising_twice_changes_nothing() {
        let inputs = [
            r#"<p>a</p><script>x</script><img src="cid:y" onerror="z">"#,
            r#"<a href="https://example.com">ok</a><meta http-equiv="refresh" content="0">"#,
            r#"<table width="100%"><tr><td>x</td></tr></table>"#,
        ];
        for input in inputs {
            let once = sanitize_mail_html(input);
            assert_eq!(sanitize_mail_html(&once), once, "not idempotent: {input}");
        }
    }

    #[test]
    fn a_lone_angle_bracket_is_re_escaped() {
        // `3 < 5` must not become an opening tag for whatever follows.
        assert_eq!(sanitize_mail_html("3 < 5"), "3 &lt; 5");
        // An unterminated `<b` *is* treated as a tag by real parsers, so it is dropped rather than
        // re-escaped. Both answers are safe; this pins which one the code gives.
        assert_eq!(sanitize_mail_html("a<b"), "a");
    }

    #[test]
    fn the_url_check_accepts_what_it_should() {
        for safe in [
            "https://example.com",
            "http://example.com/a?b=c",
            "cid:logo123",
            "data:image/png;base64,AAAA",
            "mailto:someone@example.com",
            "javascript-guide.html",
            "/relative/path",
        ] {
            assert!(!is_dangerous_url(safe), "must be allowed: {safe}");
        }
        for bad in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "  javascript:alert(1)",
            "java\nscript:alert(1)",
            "vbscript:msgbox(1)",
            "file:///C:/Windows/System32/calc.exe",
            "data:text/html,<script>alert(1)</script>",
            "ms-its:help.chm::/x.htm",
        ] {
            assert!(is_dangerous_url(bad), "must be blocked: {bad}");
        }
    }

    #[test]
    fn bidi_overrides_come_out_of_file_names_too() {
        // The other consumer of `strip_invisible_controls`: a name that *displays* as `txt.exe`.
        assert_eq!(
            strip_invisible_controls("invoice\u{202E}gnp.exe"),
            "invoicegnp.exe"
        );
        assert_eq!(strip_invisible_controls("a\u{200B}b\u{FEFF}c"), "abc");
    }
}
