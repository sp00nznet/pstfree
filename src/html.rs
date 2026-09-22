//! An HTML body, reduced to the text a person wanted to read.
//!
//! A great many messages in a PST have an HTML body and no plain-text one — Outlook stops
//! writing the alternative the moment the message is composed as rich text. Until now the
//! window said so and offered export instead, which is a fair answer for a reader that
//! cannot render markup and a poor one for anybody who just wants to know what the
//! message said.
//!
//! This is not a renderer and is not trying to be. Rendering HTML means hosting a browser
//! control — a whole other runtime, on a machine where the point of the program is that
//! there is nothing to install — or writing a layout engine, which is a larger project
//! than reading PST files. What it does instead is throw the markup away and keep the
//! text, breaking lines where the markup says a line breaks.
//!
//! The rules are deliberately small, because every rule here is a guess about intent and
//! a guess that fires rarely is a bug nobody finds:
//!
//! - `<script>` and `<style>` have their contents dropped, not just their tags. Their
//!   contents are the one part of an HTML document that is definitely not prose.
//! - The block-level tags break a line; everything else is removed and leaves its text.
//! - Entities are decoded, named and numeric both.
//! - Runs of blank lines collapse to one, because markup indentation becomes whitespace.
//!
//! What is knowingly lost: tables become their cells one after another, images leave
//! nothing behind unless they carry `alt` text, and link targets are dropped — the text
//! of the link is kept, the URL is not. Export to `.eml` and open it in a mail client to
//! see any of that; this is the reading pane, not the archive.

/// Turn an HTML body into readable plain text, with `\n` line breaks.
pub fn to_text(html: &str) -> String {
    let b = html.as_bytes();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;

    while i < b.len() {
        if b[i] != b'<' {
            let start = i;
            while i < b.len() && b[i] != b'<' && b[i] != b'&' {
                i += 1;
            }
            push_text(&mut out, &html[start..i]);
            if i < b.len() && b[i] == b'&' {
                i += entity(&html[i..], &mut out);
            }
            continue;
        }

        let Some(end) = memchr(b, i + 1, b'>') else {
            // An unterminated `<` is the rest of the document. Nothing good is in it, and
            // a reader that hangs on to it prints raw markup at the end of every message.
            break;
        };
        let tag = &html[i + 1..end];
        i = end + 1;

        let name = tag_name(tag);
        match name.as_str() {
            // Contents dropped along with the tags: skip to the closer, or to the end if
            // there isn't one, which is what a browser does with an unclosed <script>.
            "script" | "style" | "head" if !tag.starts_with('/') => {
                let close = format!("</{name}");
                i = match html[i..].to_ascii_lowercase().find(&close) {
                    Some(at) => i + at,
                    None => b.len(),
                };
            }
            // An image is only worth a line if it says what it was.
            "img" => {
                if let Some(alt) = attr(tag, "alt") {
                    if !alt.trim().is_empty() {
                        push_text(&mut out, &format!("[{}]", alt.trim()));
                    }
                }
            }
            // `<br>` is explicit, so two of them make a blank line and both are kept. A
            // block tag breaks only when there is something to break after it: `</p><p>`
            // is one line ending, not two, and real mail nests block tags deeply enough
            // that the difference is a readable message against a page of gaps.
            "br" => out.push('\n'),
            // A cell break is a space, not a newline: a table row read down one cell per
            // line is far harder to follow than the same row read across.
            "td" | "th" | "/td" | "/th" => push_text(&mut out, " "),
            _ if BREAKS.contains(&name.trim_start_matches('/'))
                && !out.is_empty()
                && !out.ends_with('\n') =>
            {
                out.push('\n');
            }
            _ => {}
        }
    }

    tidy(&out)
}

/// Tags after which the text starts on a new line. Opening or closing either one.
const BREAKS: &[&str] = &[
    "p",
    "div",
    "tr",
    "li",
    "ul",
    "ol",
    "table",
    "blockquote",
    "pre",
    "hr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "section",
    "article",
    "header",
    "footer",
    "figure",
    "dl",
    "dt",
    "dd",
    "form",
    "fieldset",
    "address",
    "body",
];

fn memchr(b: &[u8], from: usize, needle: u8) -> Option<usize> {
    b[from.min(b.len())..]
        .iter()
        .position(|&c| c == needle)
        .map(|p| p + from)
}

/// The tag's name, lowercased, with a leading `/` kept so a closer can be told apart.
fn tag_name(tag: &str) -> String {
    let t = tag.trim_start();
    let closing = t.starts_with('/');
    let t = t.trim_start_matches('/');
    let end = t
        .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .unwrap_or(t.len());
    let mut name = String::new();
    if closing {
        name.push('/');
    }
    name.push_str(&t[..end].to_ascii_lowercase());
    name
}

/// One attribute's value, single- or double-quoted. Unquoted values are not read: they
/// cannot contain the text worth having here, and parsing them is where HTML gets hairy.
fn attr(tag: &str, want: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(want) {
        let at = from + at;
        let rest = tag[at + want.len()..].trim_start();
        // `alt=` and not `salt=`: the character before has to be a separator.
        let before_ok = at == 0 || !tag.as_bytes()[at - 1].is_ascii_alphanumeric();
        if before_ok && rest.starts_with('=') {
            let v = rest[1..].trim_start();
            let quote = v.chars().next()?;
            if quote == '"' || quote == '\'' {
                let end = v[1..].find(quote)?;
                let mut out = String::new();
                let mut s = &v[1..1 + end];
                while let Some(amp) = s.find('&') {
                    out.push_str(&s[..amp]);
                    let used = entity(&s[amp..], &mut out);
                    s = &s[amp + used..];
                }
                out.push_str(s);
                return Some(out);
            }
        }
        from = at + want.len();
    }
    None
}

/// Decode one entity at the start of `s`, appending it, and say how many bytes it used.
/// A `&` that starts nothing recognisable is a literal ampersand, which is common enough
/// in real mail that dropping it would be the more visible bug.
fn entity(s: &str, out: &mut String) -> usize {
    let Some(end) = s[1..]
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '#')
        .map(|p| p + 1)
    else {
        out.push('&');
        return 1;
    };
    if s.as_bytes().get(end) != Some(&b';') || end == 1 {
        out.push('&');
        return 1;
    }

    let name = &s[1..end];
    let ch = if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => num.parse::<u32>().ok(),
        };
        code.and_then(char::from_u32)
    } else {
        named(&name.to_ascii_lowercase())
    };

    match ch {
        Some(c) => {
            // A non-breaking space is a space here: keeping U+00A0 means a terminal or an
            // EDIT control shows it as a box, and nobody wanted a box.
            out.push(if c == '\u{A0}' { ' ' } else { c });
            end + 1
        }
        None => {
            out.push('&');
            1
        }
    }
}

/// The named entities that actually turn up in mail. The full list is 2231 names and
/// most of them have never been typed by a human being.
fn named(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{A0}',
        "ndash" => '–',
        "mdash" => '—',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "hellip" => '…',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "deg" => '°',
        "plusmn" => '±',
        "times" => '×',
        "divide" => '÷',
        "frac12" => '½',
        "laquo" => '«',
        "raquo" => '»',
        "dagger" => '†',
        "sect" => '§',
        "para" => '¶',
        "shy" => '\u{AD}',
        _ => return None,
    })
}

/// Append text, collapsing the whitespace markup left behind. Every newline in the source
/// is indentation; the real line breaks come from the tags.
fn push_text(out: &mut String, s: &str) {
    for c in s.chars() {
        if c.is_whitespace() {
            if !out.ends_with([' ', '\n']) {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
}

/// Trailing spaces off each line, and no more than one blank line in a row.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0;
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

/// Bytes out of a PST that are text in some code page — `PidTagHtml`, or any
/// `PtypString8` property — as a `String`.
///
/// The file stores them in whatever code page it was written with and does not reliably
/// record which, and this project carries no character-set library. UTF-8 is tried first
/// because it is what most of the last fifteen years of mail is; anything else is read as
/// windows-1252, which is iso-8859-1 with the 0x80–0x9F range filled in — the curly
/// quotes and dashes Word puts in a message, and the bytes that are otherwise simply
/// wrong rather than merely odd.
///
// ponytail: two encodings, not a charset library. A shift_jis or gbk body decodes as
// mojibake here and correctly in an exported .eml, which declares the real charset. Add a
// decoder if a file ever turns up where that matters.
pub fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| cp1252(b)).collect(),
    }
}

fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž',
        '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}',
        'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_becomes_text() {
        let html = "<html><head><title>x</title><style>p{color:red}</style></head>\
                    <body><p>Hello <b>world</b>.</p><p>Second&nbsp;line &amp; more.</p></body>";
        assert_eq!(to_text(html), "Hello world.\nSecond line & more.");
    }

    #[test]
    fn script_and_style_contents_are_dropped() {
        let html = "<script>var a = 1 < 2;</script><style>a{b:c}</style><p>kept</p>";
        assert_eq!(to_text(html), "kept");
    }

    #[test]
    fn breaks_come_from_the_tags_not_the_source_newlines() {
        // Indentation in the source is whitespace; only <br> and the block tags break.
        let html = "<div>one\n   two</div><div>three<br>four</div>";
        assert_eq!(to_text(html), "one two\nthree\nfour");
    }

    #[test]
    fn a_row_reads_across_and_rows_read_down() {
        let html = "<table><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table>";
        assert_eq!(to_text(html), "a b\nc");
    }

    #[test]
    fn entities_named_numeric_and_broken() {
        let mut s = String::new();
        entity("&#8217;x", &mut s);
        assert_eq!(s, "’");
        assert_eq!(to_text("<p>&#x41;&#66;&rsquo;</p>"), "AB’");
        // Bare ampersands survive: "Marks & Spencer" is not an entity and must not vanish.
        assert_eq!(
            to_text("<p>Marks & Spencer &notanentity;</p>"),
            "Marks & Spencer &notanentity;"
        );
    }

    #[test]
    fn an_image_leaves_its_alt_text_and_nothing_else() {
        assert_eq!(
            to_text("<p><img src=\"a.png\"> <img src=x alt='The chart'></p>"),
            "[The chart]"
        );
    }

    #[test]
    fn blank_lines_collapse_but_one_survives() {
        // Nested block tags with nothing between them are one line ending however many
        // of them there are; explicit <br>s are what somebody typed, and those stack.
        assert_eq!(to_text("<div><p>a</p></div><div><p>b</p></div>"), "a\nb");
        assert_eq!(to_text("<p>a<br><br><br><br>b</p>"), "a\n\nb");
    }

    #[test]
    fn unterminated_markup_does_not_leak_into_the_text() {
        assert_eq!(to_text("<p>text</p><div class=\"unclosed"), "text");
    }

    #[test]
    fn decode_prefers_utf8_and_falls_back_to_cp1252() {
        assert_eq!(decode("don’t".as_bytes()), "don’t");
        // The same apostrophe as a single windows-1252 byte, which is not valid UTF-8.
        assert_eq!(decode(b"don\x92t"), "don’t");
        assert_eq!(decode(b"\x80"), "€");
    }
}
