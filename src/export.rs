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

use crate::cfbf::{self, Item};
use crate::ltp::{
    asctime, read_node_pc, read_recipients, Pc, Recipient, NID_ROOT_FOLDER, PID_DISPLAY_NAME,
    PID_EMAIL_ADDRESS, PID_MESSAGE_SIZE, PID_RECIPIENT_TYPE, PID_SMTP_ADDRESS,
};
use crate::ltp::{
    clean_subject, read_pc, rfc5322_date, Value, ATTACH_BY_VALUE, NID_TYPE_ATTACHMENT,
    PID_ATTACH_DATA, PID_ATTACH_FILENAME, PID_ATTACH_LONG_FILENAME, PID_ATTACH_METHOD,
    PID_ATTACH_MIME_TAG, PID_BODY, PID_BODY_HTML, PID_DELIVERY_TIME, PID_DISPLAY_CC,
    PID_DISPLAY_TO, PID_INTERNET_CODEPAGE, PID_INTERNET_MSG_ID, PID_SENDER_EMAIL, PID_SENDER_NAME,
    PID_SUBJECT, PID_SUBMIT_TIME, PID_TRANSPORT_HEADERS,
};
use crate::ndb::{Node, Pst};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What to write the mail out as.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// RFC 5322 with MIME. Opens anywhere.
    Eml,
    /// One file per folder, messages concatenated. What mail archives are kept in.
    Mbox,
    /// Outlook's own compound-file format. Keeps the MAPI properties a mail message
    /// cannot carry.
    Msg,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "eml" => Some(Format::Eml),
            "mbox" => Some(Format::Mbox),
            "msg" => Some(Format::Msg),
            _ => None,
        }
    }
}

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
    format: Format,
    on: crate::Progress,
) -> Stats {
    let mut st = Stats {
        messages: 0,
        attachments: 0,
        failed: 0,
        errors: Vec::new(),
    };
    let paths = folder_paths(nodes, folder_names, root);
    let total = nodes.iter().filter(|n| n.nid_type() == 0x04).count() as u64;
    let mut done = 0u64;

    for n in nodes.iter().filter(|n| n.nid_type() == 0x04) {
        done += 1;
        on(done, total);
        // A message whose folder is missing still gets written out. In a damaged file it
        // is the one most worth keeping, so it must not be the one that is dropped.
        let dir = paths
            .get(&n.nid_parent)
            .cloned()
            .unwrap_or_else(|| root.join("_no-folder"));

        match write_message(pst, n, &dir, format) {
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

fn write_message(pst: &mut Pst, node: &Node, dir: &Path, format: Format) -> Result<usize, String> {
    let pc = read_node_pc(pst, node)?;
    let attachments = collect_attachments(pst, node);
    let recipients = read_recipients(pst, node.bid_sub);
    let subject = clean_subject(pc.str(PID_SUBJECT).unwrap_or("(no subject)"));

    if format == Format::Mbox {
        // One file per folder rather than one per message, so the folder's own directory
        // is not created at all - the mbox takes its place beside where it would have been.
        let parent = dir.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        let path = PathBuf::from(format!("{}.mbox", dir.display()));

        let eml = build_eml(&pc, &attachments, &recipients);
        let when = pc
            .time(PID_DELIVERY_TIME)
            .or(pc.time(PID_SUBMIT_TIME))
            .unwrap_or(0);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        std::io::Write::write_all(&mut f, &mbox_entry(&eml, when))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(attachments.len());
    }

    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (ext, bytes) = match format {
        Format::Msg => ("msg", build_msg(&pc, &attachments, &recipients)),
        _ => ("eml", build_eml(&pc, &attachments, &recipients)),
    };
    let base = safe_name(subject, node.nid);

    // Two messages in one folder very often share a subject, and writing both to the same
    // name would silently destroy one of them. The node id is unique within the file, so
    // one fallback is always enough.
    let mut path = dir.join(format!("{base}.{ext}"));
    if path.exists() {
        path = dir.join(format!("{base} (0x{:X}).{ext}", node.nid));
    }
    std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(attachments.len())
}

/// One message as it appears inside an mbox file.
///
/// The separator is a line beginning `From `, which means any line of the message that
/// also begins that way has to be escaped or it would look like the start of the next
/// message. Escaping `>From ` and `>>From ` too is the mboxrd convention, and it is the
/// one that can be undone exactly.
fn mbox_entry(eml: &[u8], when: u64) -> Vec<u8> {
    let mut out = format!("From pstfree@localhost {}\r\n", asctime(when)).into_bytes();
    for line in eml.split(|&b| b == b'\n') {
        let bare = line.strip_prefix(b">").map_or(line, |l| {
            let mut l = l;
            while let Some(r) = l.strip_prefix(b">") {
                l = r;
            }
            l
        });
        if bare.starts_with(b"From ") {
            out.push(b'>');
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.extend_from_slice(b"\n");
    out
}

pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
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

pub fn build_eml(pc: &Pc, attachments: &[Attachment], recipients: &[Recipient]) -> Vec<u8> {
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
        None => synthesize_headers(pc, recipients, &mut out),
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
        // PidTagHtml does not always hold HTML. Outlook's own account-test message puts a
        // plain-text body in it, and declaring that text/html makes a reader fold the
        // blank lines away — the only structure a plain-text body has. Nothing without a
        // single `<` anywhere in it is markup, whichever property it arrived in.
        let kind = if b.contains(&b'<') {
            "text/html"
        } else {
            "text/plain"
        };
        parts.push((format!("{kind}; charset={cs}"), b));
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

/// Properties for one object in a `.msg`: the fixed-size ones packed into a table, the
/// variable ones written out as their own streams.
#[derive(Default)]
struct Props {
    entries: Vec<u8>,
    streams: Vec<Item>,
}

impl Props {
    fn fixed(&mut self, prop: u16, ptype: u16, value: u64) {
        self.entries
            .extend_from_slice(&(((prop as u32) << 16) | ptype as u32).to_le_bytes());
        self.entries.extend_from_slice(&6u32.to_le_bytes()); // readable and writable
        self.entries.extend_from_slice(&value.to_le_bytes());
    }

    /// A variable-length value: the table records its size, and the bytes go in a stream
    /// named for the property.
    fn variable(&mut self, prop: u16, ptype: u16, bytes: Vec<u8>, declared: usize) {
        self.entries
            .extend_from_slice(&(((prop as u32) << 16) | ptype as u32).to_le_bytes());
        self.entries.extend_from_slice(&6u32.to_le_bytes());
        self.entries
            .extend_from_slice(&(declared as u32).to_le_bytes());
        self.entries.extend_from_slice(&0u32.to_le_bytes());
        self.streams.push(Item::Stream(
            format!("__substg1.0_{prop:04X}{ptype:04X}"),
            bytes,
        ));
    }

    fn string(&mut self, prop: u16, s: &str) {
        if s.is_empty() {
            return;
        }
        let utf16: Vec<u8> = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        // The recorded size counts the terminator that is not written to the stream.
        let declared = utf16.len() + 2;
        self.variable(prop, 0x001F, utf16, declared);
    }

    fn binary(&mut self, prop: u16, b: &[u8]) {
        if b.is_empty() {
            return;
        }
        self.variable(prop, 0x0102, b.to_vec(), b.len());
    }

    fn time(&mut self, prop: u16, ft: u64) {
        if ft != 0 {
            self.fixed(prop, 0x0040, ft);
        }
    }

    /// The property table stream, with the header the object's kind requires in front.
    fn finish(mut self, header: Vec<u8>) -> Vec<Item> {
        let mut table = header;
        table.extend_from_slice(&self.entries);
        self.streams
            .push(Item::Stream("__properties_version1.0".into(), table));
        self.streams
    }
}

/// Build a `.msg`: Outlook's own format, which keeps the MAPI properties that a mail
/// message has nowhere to put.
///
/// Strings are written as Unicode throughout, and `PidTagStoreSupportMask` says so — a
/// reader uses that flag to decide how to interpret every string in the file, so getting
/// it wrong turns the whole message into mojibake rather than failing outright.
pub fn build_msg(pc: &Pc, attachments: &[Attachment], recipients: &[Recipient]) -> Vec<u8> {
    let mut p = Props::default();
    const STORE_UNICODE_OK: u64 = 0x0004_0000;
    p.fixed(0x340D, 0x0003, STORE_UNICODE_OK);
    p.string(0x001A, pc.str(0x001A).unwrap_or("IPM.Note"));
    p.string(
        PID_SUBJECT,
        clean_subject(pc.str(PID_SUBJECT).unwrap_or("")),
    );
    p.string(PID_SENDER_NAME, pc.str(PID_SENDER_NAME).unwrap_or(""));
    p.string(PID_SENDER_EMAIL, pc.str(PID_SENDER_EMAIL).unwrap_or(""));
    p.string(PID_DISPLAY_TO, pc.str(PID_DISPLAY_TO).unwrap_or(""));
    p.string(PID_DISPLAY_CC, pc.str(PID_DISPLAY_CC).unwrap_or(""));
    p.string(
        PID_TRANSPORT_HEADERS,
        pc.str(PID_TRANSPORT_HEADERS).unwrap_or(""),
    );
    p.string(
        PID_INTERNET_MSG_ID,
        pc.str(PID_INTERNET_MSG_ID).unwrap_or(""),
    );
    p.string(PID_BODY, pc.str(PID_BODY).unwrap_or(""));
    if let Some(Value::Bytes(h)) = pc.props.get(&PID_BODY_HTML) {
        p.binary(PID_BODY_HTML, h);
    }
    p.time(PID_DELIVERY_TIME, pc.time(PID_DELIVERY_TIME).unwrap_or(0));
    p.time(PID_SUBMIT_TIME, pc.time(PID_SUBMIT_TIME).unwrap_or(0));
    if let Some(n) = pc.int(PID_MESSAGE_SIZE) {
        p.fixed(PID_MESSAGE_SIZE, 0x0003, n as u32 as u64);
    }

    // A message's header records how many recipients and attachments follow, and what id
    // the next one would take.
    let mut header = vec![0u8; 8];
    header.extend_from_slice(&(recipients.len() as u32).to_le_bytes());
    header.extend_from_slice(&(attachments.len() as u32).to_le_bytes());
    header.extend_from_slice(&(recipients.len() as u32).to_le_bytes());
    header.extend_from_slice(&(attachments.len() as u32).to_le_bytes());
    header.extend_from_slice(&[0u8; 8]);

    let mut items = p.finish(header);

    for (i, r) in recipients.iter().enumerate() {
        let mut rp = Props::default();
        rp.fixed(PID_RECIPIENT_TYPE, 0x0003, r.kind as u64);
        rp.string(PID_DISPLAY_NAME, &r.name);
        rp.string(PID_EMAIL_ADDRESS, &r.email);
        rp.string(PID_SMTP_ADDRESS, &r.email);
        rp.string(0x3002, "SMTP"); // PidTagAddressType
        items.push(Item::Storage(
            format!("__recip_version1.0_#{i:08X}"),
            rp.finish(vec![0u8; 8]),
        ));
    }

    for (i, a) in attachments.iter().enumerate() {
        let mut ap = Props::default();
        ap.fixed(PID_ATTACH_METHOD, 0x0003, ATTACH_BY_VALUE as u64);
        ap.fixed(0x0E20, 0x0003, a.data.len() as u64); // PidTagAttachSize
        ap.string(PID_ATTACH_LONG_FILENAME, &a.name);
        ap.string(PID_ATTACH_FILENAME, &a.name);
        ap.string(PID_ATTACH_MIME_TAG, &a.mime);
        ap.binary(PID_ATTACH_DATA, &a.data);
        items.push(Item::Storage(
            format!("__attach_version1.0_#{i:08X}"),
            ap.finish(vec![0u8; 8]),
        ));
    }

    cfbf::build(items)
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

fn synthesize_headers(pc: &Pc, recipients: &[Recipient], out: &mut String) {
    let from = match (pc.str(PID_SENDER_NAME), pc.str(PID_SENDER_EMAIL)) {
        (Some(n), Some(e)) if e.contains('@') => format!("{} <{e}>", encode_header(n)),
        (Some(n), _) => encode_header(n),
        (None, Some(e)) => e.to_string(),
        _ => String::new(),
    };
    if !from.is_empty() {
        out.push_str(&format!("From: {from}\r\n"));
    }
    // Real addressees from the recipient table where there is one. The display-name
    // properties are the fallback: they hold what Outlook showed, not where it was sent.
    for (kind, tag, fallback) in [
        (1, "To", PID_DISPLAY_TO),
        (2, "Cc", PID_DISPLAY_CC),
        (3, "Bcc", 0),
    ] {
        let listed: Vec<String> = recipients
            .iter()
            .filter(|r| r.kind == kind)
            .map(|r| match (r.name.trim(), r.email.trim()) {
                ("", e) => e.to_string(),
                (n, "") => encode_header(n),
                (n, e) => format!("{} <{e}>", encode_header(n)),
            })
            .filter(|s| !s.is_empty())
            .collect();

        if !listed.is_empty() {
            out.push_str(&format!("{tag}: {}\r\n", listed.join(", ")));
        } else if fallback != 0 {
            if let Some(v) = pc.str(fallback).filter(|v| !v.trim().is_empty()) {
                out.push_str(&format!("{tag}: {}\r\n", encode_header(v)));
            }
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
        Pc {
            props: props.iter().cloned().collect(),
            from_subnode: 0,
        }
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
        let eml = String::from_utf8(build_eml(&pc, &[], &[])).unwrap();
        let head = eml.split("\r\n\r\n").next().unwrap();

        assert!(
            !head.contains("OLDBOUNDARY"),
            "orphan boundary survived:\n{head}"
        );
        assert_eq!(head.matches("MIME-Version").count(), 1, "{head}");
        assert_eq!(head.matches("Content-Type:").count(), 1, "{head}");
        assert!(
            head.contains("From: a@b.c"),
            "real headers were lost:\n{head}"
        );
        assert!(
            head.contains("Subject: hi"),
            "real headers were lost:\n{head}"
        );
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
        let plain = String::from_utf8(build_eml(&pc, &[], &[])).unwrap();
        assert!(plain.contains("multipart/alternative"), "{plain}");
        assert!(plain.contains("text/plain") && plain.contains("text/html"));

        let att = [Attachment {
            name: "a.txt".into(),
            mime: "text/plain".into(),
            data: b"hello".to_vec(),
        }];
        let mixed = String::from_utf8(build_eml(&pc, &att, &[])).unwrap();
        assert!(mixed.contains("multipart/mixed"), "{mixed}");
        assert!(mixed.contains("filename=\"a.txt\""));
        // aGVsbG8= is "hello". The attachment bytes must actually be in there.
        assert!(mixed.contains("aGVsbG8="), "attachment payload missing");
        assert!(mixed.trim_end().ends_with("--"), "multipart is not closed");
    }

    /// A body is typed by what it contains, not by the property it arrived in.
    ///
    /// The 2013 OST fixture stores its message body in `PidTagHtml` with no markup in it
    /// at all — libpff reads the same bytes, so this is what Outlook wrote, not a
    /// misread. Sending that out as `text/html` costs the reader every line break in it.
    #[test]
    fn a_body_with_no_markup_is_not_html_whatever_property_it_came_from() {
        let plain = pc_with(&[(
            PID_BODY_HTML,
            Value::Bytes(b"Line one.\r\n\r\nLine two.\r\n".to_vec()),
        )]);
        let out = String::from_utf8(build_eml(&plain, &[], &[])).unwrap();
        assert!(
            out.contains("text/plain"),
            "no-markup body typed as HTML:\n{out}"
        );
        assert!(!out.contains("text/html"), "{out}");

        let markup = pc_with(&[(PID_BODY_HTML, Value::Bytes(b"<p>rich</p>".to_vec()))]);
        let out = String::from_utf8(build_eml(&markup, &[], &[])).unwrap();
        assert!(
            out.contains("text/html"),
            "real markup must stay HTML:\n{out}"
        );
    }

    /// A line beginning `From ` inside a message would look like the start of the next
    /// one, so it has to be escaped - and lines that already begin with `>From ` too, or
    /// unescaping afterwards would strip a `>` the sender actually wrote.
    #[test]
    fn mbox_escapes_lines_that_look_like_separators() {
        let body = b"Subject: x\n\nFrom here on\n>From a quote\n>>From deeper\nnot From here\n";
        let out = String::from_utf8(mbox_entry(body, 0)).unwrap();
        let lines: Vec<&str> = out.lines().collect();

        assert!(
            lines[0].starts_with("From pstfree@localhost "),
            "no separator: {:?}",
            lines[0]
        );
        assert!(
            lines.contains(&">From here on"),
            "unescaped separator-alike:\n{out}"
        );
        assert!(
            lines.contains(&">>From a quote"),
            "quoted form not escaped:\n{out}"
        );
        assert!(
            lines.contains(&">>>From deeper"),
            "double-quoted form not escaped:\n{out}"
        );
        assert!(
            lines.contains(&"not From here"),
            "escaped something mid-line:\n{out}"
        );
        // Exactly one separator, or a reader would see two messages.
        assert_eq!(lines.iter().filter(|l| l.starts_with("From ")).count(), 1);
    }

    /// The compound-file layer has its own round-trip tests; this checks that a message
    /// actually comes out as one, with the streams a reader looks for.
    #[test]
    fn msg_is_a_compound_file_with_the_expected_streams() {
        let pc = pc_with(&[
            (PID_SUBJECT, Value::Str("hello".into())),
            (PID_SENDER_NAME, Value::Str("Someone".into())),
        ]);
        let recipients = [Recipient {
            kind: 1,
            name: "A Person".into(),
            email: "a@b.c".into(),
        }];
        let msg = build_msg(&pc, &[], &recipients);

        assert_eq!(
            &msg[0..8],
            &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1],
            "not a compound file"
        );
        assert_eq!(msg.len() % 512, 0, "not a whole number of sectors");

        // Directory entry names are UTF-16, so the names appear in the bytes that way.
        let as_utf16 =
            |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect() };
        for name in [
            "__properties_version1.0",
            "__substg1.0_0037001F",
            "__recip_version1.0_#00000000",
        ] {
            let needle = as_utf16(name);
            assert!(
                msg.windows(needle.len()).any(|w| w == needle),
                "no {name} in the file"
            );
        }
        // And the subject itself is in there as Unicode.
        let subject = as_utf16("hello");
        assert!(
            msg.windows(subject.len()).any(|w| w == subject),
            "subject missing"
        );
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
