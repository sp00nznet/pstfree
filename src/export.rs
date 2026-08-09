//! Writing messages back out as `.eml` files.
//!
//! The output is RFC 5322 with MIME, which is what Thunderbird, Outlook, `mutt` and
//! everything else will open. One file per message, one directory per folder.
//!
//! Two rules run through all of it. Nothing invents information: no timezone is claimed
//! that the file does not record, and a body of unknown character set is declared as what
//! it says it is rather than transcoded on a guess. And nothing stops: one unreadable
//! message does not end the export, because the file that needs exporting is the broken
//! one.

use crate::ltp::{
    clean_subject, read_pc, rfc5322_date, Value, ATTACH_BY_VALUE, NID_TYPE_ATTACHMENT,
    PID_ATTACH_DATA, PID_ATTACH_FILENAME, PID_ATTACH_LONG_FILENAME, PID_ATTACH_METHOD,
    PID_ATTACH_MIME_TAG, PID_BODY, PID_BODY_HTML, PID_DELIVERY_TIME, PID_DISPLAY_CC,
    PID_DISPLAY_TO, PID_INTERNET_CODEPAGE, PID_INTERNET_MSG_ID, PID_SENDER_EMAIL, PID_SENDER_NAME,
    PID_SUBJECT, PID_SUBMIT_TIME, PID_TRANSPORT_HEADERS,
};
use crate::ltp::{read_node_pc, Pc, NID_ROOT_FOLDER};
use crate::ndb::{Node, Pst};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Stats {
    pub messages: usize,
    pub attachments: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

/// Write every message in the file to `root`, one directory per folder.
pub fn export(
    pst: &mut Pst,
    nodes: &[Node],
    folder_names: &BTreeMap<u32, String>,
    root: &Path,
) -> Stats {
    let mut st = Stats {
        messages: 0,
        attachments: 0,
        failed: 0,
        errors: Vec::new(),
    };
    let paths = folder_paths(nodes, folder_names, root);

    for n in nodes.iter().filter(|n| n.nid_type() == 0x04) {
        // A message whose folder is missing still gets written out. In a damaged file it
        // is the one most worth keeping, so it must not be the one that is dropped.
        let dir = paths
            .get(&n.nid_parent)
            .cloned()
            .unwrap_or_else(|| root.join("_no-folder"));

        match write_message(pst, n, &dir) {
            Ok(attached) => {
                st.messages += 1;
                st.attachments += attached;
            }
            Err(e) => {
                st.failed += 1;
                st.errors.push(format!("message 0x{:X}: {e}", n.nid));
            }
        }
    }
    st
}

/// A directory path for each folder, mirroring the tree.
fn folder_paths(
    nodes: &[Node],
    names: &BTreeMap<u32, String>,
    root: &Path,
) -> BTreeMap<u32, PathBuf> {
    let parent: BTreeMap<u32, u32> = nodes
        .iter()
        .filter(|n| n.nid_type() == 0x02)
        .map(|n| (n.nid, n.nid_parent))
        .collect();

    let mut out = BTreeMap::new();
    for &nid in names.keys() {
        // Walk up to the root, guarding against a parent chain that loops back on itself.
        let mut chain = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cur = nid;
        while seen.insert(cur) {
            // The root folder has no name and is not a folder anyone sees, so it
            // contributes no directory level - only the named folders under it do.
            if cur != NID_ROOT_FOLDER {
                if let Some(name) = names.get(&cur) {
                    chain.push(safe_name(name, cur));
                }
            }
            match parent.get(&cur) {
                Some(&p) if p != cur => cur = p,
                _ => break,
            }
        }
        chain.reverse();
        out.insert(nid, chain.iter().fold(root.to_path_buf(), |p, c| p.join(c)));
    }
    out
}

/// Turn a folder or subject into something Windows will accept as a name.
///
/// This is a trust boundary: the text comes from the file, so it can contain separators,
/// `..`, control characters, or a reserved device name. None of those may reach the
/// filesystem.
fn safe_name(s: &str, unique: u32) -> String {
    let mut out: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();

    // Windows silently strips trailing dots and spaces, which would let "a." and "a"
    // collide, and refuses these names outright whatever the extension.
    out = out.trim().trim_end_matches('.').trim().to_string();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if out.is_empty() || RESERVED.contains(&out.to_ascii_uppercase().as_str()) {
        return format!("{out}_{unique:X}");
    }

    // Long names are truncated on a character boundary, then made unique again, because
    // two different long subjects usually share a prefix.
    if out.chars().count() > 60 {
        out = out.chars().take(60).collect::<String>();
        return format!("{}_{unique:X}", out.trim_end());
    }
    out
}

fn write_message(pst: &mut Pst, node: &Node, dir: &Path) -> Result<usize, String> {
    let pc = read_node_pc(pst, node)?;
    let attachments = collect_attachments(pst, node);
    let eml = build_eml(&pc, &attachments);

    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let subject = clean_subject(pc.str(PID_SUBJECT).unwrap_or("(no subject)"));
    let base = safe_name(subject, node.nid);

    // Two messages in one folder very often share a subject, and writing both to the same
    // name would silently destroy one of them. The node id is unique within the file, so
    // one fallback is always enough.
    let mut path = dir.join(format!("{base}.eml"));
    if path.exists() {
        path = dir.join(format!("{base} (0x{:X}).eml", node.nid));
    }
    std::fs::write(&path, eml).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(attachments.len())
}

struct Attachment {
    name: String,
    mime: String,
    data: Vec<u8>,
}

/// Attachments hang off the message as subnodes.
///
/// ponytail: enumerated straight from the subnode tree by node type, rather than by
/// reading the message's attachment table. The table is a table context and this is not;
/// both list the same attachments, and when they disagree the file is damaged - which is
/// a reason to have both eventually, not a reason to implement the harder one first.
fn collect_attachments(pst: &mut Pst, node: &Node) -> Vec<Attachment> {
    let Ok(subs) = pst.subnodes(node.bid_sub) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    for (&nid, &sub) in subs.iter() {
        if nid & 0x1F != NID_TYPE_ATTACHMENT {
            continue;
        }
        let Ok(pc) = read_pc(pst, sub.data, sub.sub) else {
            continue;
        };
        // Anything other than by-value is a link or an embedded message, and there are no
        // bytes here to write. Skipped rather than written empty.
        if pc.int(PID_ATTACH_METHOD).unwrap_or(ATTACH_BY_VALUE) != ATTACH_BY_VALUE {
            continue;
        }
        let Some(Value::Bytes(data)) = pc.props.get(&PID_ATTACH_DATA) else {
            continue;
        };

        let name = pc
            .str(PID_ATTACH_LONG_FILENAME)
            .or(pc.str(PID_ATTACH_FILENAME))
            .unwrap_or("attachment")
            .to_string();
        out.push(Attachment {
            mime: pc
                .str(PID_ATTACH_MIME_TAG)
                .unwrap_or("application/octet-stream")
                .to_string(),
            name: safe_name(&name, nid),
            data: data.clone(),
        });
    }
    out
}

fn build_eml(pc: &Pc, attachments: &[Attachment]) -> Vec<u8> {
    let mut out = String::new();

    // The original headers are the most faithful thing in the file - real addresses, real
    // Message-ID, the route it took. Reused when present, minus anything describing a
    // body layout that is about to be rebuilt.
    match pc.str(PID_TRANSPORT_HEADERS) {
        Some(h) => {
            // A header can be folded over several lines, so dropping one means dropping
            // its continuations too. Keeping them would leave an orphan `boundary="..."`
            // line describing a body layout that no longer exists.
            let mut dropping = false;
            for line in h.lines() {
                if line.starts_with([' ', '\t']) {
                    if dropping {
                        continue;
                    }
                } else {
                    let lower = line.to_ascii_lowercase();
                    dropping = lower.starts_with("content-type:")
                        || lower.starts_with("content-transfer-encoding:")
                        || lower.starts_with("mime-version:");
                    if dropping {
                        continue;
                    }
                }
                if !line.trim().is_empty() {
                    out.push_str(line.trim_end());
                    out.push_str("\r\n");
                }
            }
        }
        None => synthesize_headers(pc, &mut out),
    }

    let text = pc
        .str(PID_BODY)
        .map(|s| (s.as_bytes().to_vec(), "utf-8".to_string()));
    let html = match pc.props.get(&PID_BODY_HTML) {
        Some(Value::Bytes(b)) => Some((b.clone(), charset(pc))),
        Some(Value::Str(s)) => Some((s.as_bytes().to_vec(), "utf-8".to_string())),
        _ => None,
    };

    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some((b, cs)) = text {
        parts.push((format!("text/plain; charset={cs}"), b));
    }
    if let Some((b, cs)) = html {
        parts.push((format!("text/html; charset={cs}"), b));
    }

    out.push_str("MIME-Version: 1.0\r\n");

    // Every payload is base64, so no boundary can occur inside one.
    let alt = "----=_pstfree_alt";
    let mix = "----=_pstfree_mix";

    if attachments.is_empty() && parts.len() <= 1 {
        let (ct, body) = parts
            .pop()
            .unwrap_or_else(|| ("text/plain; charset=utf-8".into(), Vec::new()));
        out.push_str(&format!(
            "Content-Type: {ct}\r\nContent-Transfer-Encoding: base64\r\n\r\n"
        ));
        base64(&body, &mut out);
        return out.into_bytes();
    }

    if attachments.is_empty() {
        out.push_str(&format!(
            "Content-Type: multipart/alternative; boundary=\"{alt}\"\r\n\r\n"
        ));
        write_parts(&parts, alt, &mut out);
        return out.into_bytes();
    }

    out.push_str(&format!(
        "Content-Type: multipart/mixed; boundary=\"{mix}\"\r\n\r\n"
    ));
    out.push_str(&format!("--{mix}\r\n"));
    if parts.len() > 1 {
        out.push_str(&format!(
            "Content-Type: multipart/alternative; boundary=\"{alt}\"\r\n\r\n"
        ));
        write_parts(&parts, alt, &mut out);
    } else {
        let (ct, body) = parts
            .pop()
            .unwrap_or_else(|| ("text/plain; charset=utf-8".into(), Vec::new()));
        out.push_str(&format!(
            "Content-Type: {ct}\r\nContent-Transfer-Encoding: base64\r\n\r\n"
        ));
        base64(&body, &mut out);
    }
    for a in attachments {
        out.push_str(&format!("\r\n--{mix}\r\n"));
        out.push_str(&format!(
            "Content-Type: {}; name=\"{}\"\r\n",
            a.mime, a.name
        ));
        out.push_str(&format!(
            "Content-Disposition: attachment; filename=\"{}\"\r\n",
            a.name
        ));
        out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
        base64(&a.data, &mut out);
    }
    out.push_str(&format!("\r\n--{mix}--\r\n"));
    out.into_bytes()
}

fn write_parts(parts: &[(String, Vec<u8>)], boundary: &str, out: &mut String) {
    for (ct, body) in parts {
        out.push_str(&format!("--{boundary}\r\n"));
        out.push_str(&format!(
            "Content-Type: {ct}\r\nContent-Transfer-Encoding: base64\r\n\r\n"
        ));
        base64(body, out);
        out.push_str("\r\n");
    }
    out.push_str(&format!("--{boundary}--\r\n"));
}

fn synthesize_headers(pc: &Pc, out: &mut String) {
    let from = match (pc.str(PID_SENDER_NAME), pc.str(PID_SENDER_EMAIL)) {
        (Some(n), Some(e)) if e.contains('@') => format!("{} <{e}>", encode_header(n)),
        (Some(n), _) => encode_header(n),
        (None, Some(e)) => e.to_string(),
        _ => String::new(),
    };
    if !from.is_empty() {
        out.push_str(&format!("From: {from}\r\n"));
    }
    for (tag, id) in [("To", PID_DISPLAY_TO), ("Cc", PID_DISPLAY_CC)] {
        if let Some(v) = pc.str(id).filter(|v| !v.trim().is_empty()) {
            out.push_str(&format!("{tag}: {}\r\n", encode_header(v)));
        }
    }
    if let Some(t) = pc.time(PID_DELIVERY_TIME).or(pc.time(PID_SUBMIT_TIME)) {
        out.push_str(&format!("Date: {}\r\n", rfc5322_date(t)));
    }
    if let Some(id) = pc.str(PID_INTERNET_MSG_ID) {
        out.push_str(&format!("Message-ID: {id}\r\n"));
    }
    let subject = clean_subject(pc.str(PID_SUBJECT).unwrap_or(""));
    out.push_str(&format!("Subject: {}\r\n", encode_header(subject)));
}

/// The character set an HTML body is in, as the message itself declares it.
///
/// Passed through rather than transcoded: the bytes are already correct, and re-encoding
/// them on a guess about an unrecognised code page is how mojibake gets baked in.
fn charset(pc: &Pc) -> String {
    match pc.int(PID_INTERNET_CODEPAGE) {
        Some(65001) => "utf-8",
        Some(1252) => "windows-1252",
        Some(1251) => "windows-1251",
        Some(28591) => "iso-8859-1",
        Some(28592) => "iso-8859-2",
        Some(932) => "shift_jis",
        Some(936) => "gbk",
        Some(949) => "euc-kr",
        Some(950) => "big5",
        _ => "utf-8",
    }
    .to_string()
}

/// A header value, RFC 2047 encoded if it is not plain ASCII.
fn encode_header(s: &str) -> String {
    let s = s.replace(['\r', '\n'], " ");
    if s.is_ascii() && !s.chars().any(|c| (c as u32) < 0x20) {
        return s;
    }
    let mut b64 = String::new();
    base64(s.as_bytes(), &mut b64);
    format!("=?utf-8?B?{}?=", b64.replace("\r\n", ""))
}

/// Base64, wrapped at 76 characters as MIME requires.
fn base64(data: &[u8], out: &mut String) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut col = 0;
    for c in data.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((*c.get(1).unwrap_or(&0) as u32) << 8)
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
        col += 4;
        if col >= 76 {
            out.push_str("\r\n");
            col = 0;
        }
    }
    if col > 0 {
        out.push_str("\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 4648 test vectors.
    #[test]
    fn base64_matches_the_rfc() {
        for (input, expect) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            let mut out = String::new();
            base64(input.as_bytes(), &mut out);
            assert_eq!(out.trim_end(), expect, "base64({input:?})");
        }
    }

    #[test]
    fn base64_wraps_at_76_characters() {
        let mut out = String::new();
        base64(&vec![b'x'; 300], &mut out);
        for line in out.lines() {
            assert!(
                line.len() <= 76,
                "line of {} characters: {line}",
                line.len()
            );
        }
        assert_eq!(out.lines().map(|l| l.len()).sum::<usize>(), 400);
    }

    /// The invariant that matters is not "contains no dots" - `..a` is a fine filename.
    /// It is that whatever comes back is exactly one ordinary path component, so joining
    /// it onto the output directory cannot land anywhere else.
    #[test]
    fn file_names_cannot_escape_the_output_directory() {
        for nasty in [
            "../../etc/passwd",
            "..\\..\\windows",
            "a/b",
            "a\\b",
            "C:evil",
            "..",
            ".",
            "/",
        ] {
            let s = safe_name(nasty, 1);
            let mut parts = Path::new(&s).components();
            assert!(
                matches!(parts.next(), Some(std::path::Component::Normal(_))),
                "{nasty} -> {s} is not a plain name"
            );
            assert!(
                parts.next().is_none(),
                "{nasty} -> {s} is more than one component"
            );
        }
    }

    #[test]
    fn reserved_windows_names_are_made_safe() {
        for name in ["CON", "nul", "LPT1", "  ", "", "trailing..."] {
            let s = safe_name(name, 0xAB);
            assert!(!["CON", "NUL", "LPT1"].contains(&s.to_ascii_uppercase().as_str()));
            assert!(!s.is_empty());
            assert!(!s.ends_with('.'), "{name} -> {s}");
        }
    }

    #[test]
    fn long_names_are_truncated_on_a_character_boundary() {
        let s = safe_name(&"é".repeat(200), 7);
        assert!(s.chars().count() <= 70);
        assert!(s.is_char_boundary(s.len()));
    }

    #[test]
    fn non_ascii_headers_are_encoded() {
        assert_eq!(encode_header("plain"), "plain");
        assert_eq!(encode_header("caffè"), "=?utf-8?B?Y2FmZsOo?=");
        // A newline in a subject would otherwise inject a header.
        assert!(!encode_header("a\r\nBcc: x@y").contains('\n'));
    }

    fn pc_with(props: &[(u16, Value)]) -> Pc {
        Pc { props: props.iter().cloned().collect(), from_subnode: 0 }
    }

    /// The body is rebuilt, so the original headers describing the old body layout must
    /// go - including the folded continuation lines that carry the old boundary.
    #[test]
    fn rebuilt_headers_drop_the_original_mime_layout() {
        let headers = "From: a@b.c\r\n\
             Content-Type: multipart/alternative;\r\n\
             \tboundary=\"_000_OLDBOUNDARY_\"\r\n\
             MIME-Version: 1.0\r\n\
             Subject: hi\r\n";
        let pc = pc_with(&[
            (PID_TRANSPORT_HEADERS, Value::Str(headers.into())),
            (PID_BODY, Value::Str("text".into())),
        ]);
        let eml = String::from_utf8(build_eml(&pc, &[])).unwrap();
        let head = eml.split("\r\n\r\n").next().unwrap();

        assert!(!head.contains("OLDBOUNDARY"), "orphan boundary survived:\n{head}");
        assert_eq!(head.matches("MIME-Version").count(), 1, "{head}");
        assert_eq!(head.matches("Content-Type:").count(), 1, "{head}");
        assert!(head.contains("From: a@b.c"), "real headers were lost:\n{head}");
        assert!(head.contains("Subject: hi"), "real headers were lost:\n{head}");
        // Every header line must be a name, or a continuation of one.
        for line in head.lines() {
            assert!(
                line.starts_with([' ', '\t']) || line.contains(':'),
                "not a header line: {line:?}"
            );
        }
    }

    /// Two bodies means multipart/alternative; attachments wrap that in multipart/mixed.
    #[test]
    fn builds_multipart_when_there_is_more_than_one_part() {
        let pc = pc_with(&[
            (PID_SUBJECT, Value::Str("s".into())),
            (PID_BODY, Value::Str("plain".into())),
            (PID_BODY_HTML, Value::Bytes(b"<p>rich</p>".to_vec())),
        ]);
        let plain = String::from_utf8(build_eml(&pc, &[])).unwrap();
        assert!(plain.contains("multipart/alternative"), "{plain}");
        assert!(plain.contains("text/plain") && plain.contains("text/html"));

        let att = [Attachment {
            name: "a.txt".into(),
            mime: "text/plain".into(),
            data: b"hello".to_vec(),
        }];
        let mixed = String::from_utf8(build_eml(&pc, &att)).unwrap();
        assert!(mixed.contains("multipart/mixed"), "{mixed}");
        assert!(mixed.contains("filename=\"a.txt\""));
        // aGVsbG8= is "hello". The attachment bytes must actually be in there.
        assert!(mixed.contains("aGVsbG8="), "attachment payload missing");
        assert!(mixed.trim_end().ends_with("--"), "multipart is not closed");
    }

    #[test]
    fn date_matches_rfc_5322() {
        // 2014-06-05 16:22:00Z, the delivery time of a message in the test OST.
        assert_eq!(
            rfc5322_date(130_464_589_200_000_000),
            "Thu, 5 Jun 2014 16:22:00 +0000"
        );
        // 2000-01-01 was a Saturday, and a leap year the century rule nearly skipped.
        assert_eq!(
            rfc5322_date(125_911_584_000_000_000),
            "Sat, 1 Jan 2000 00:00:00 +0000"
        );
    }
}
