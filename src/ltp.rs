//! The lists, tables and properties layer, sitting on top of the node database.
//!
//! A node's blocks hold a heap. The heap holds a B-tree. The B-tree holds properties.
//! Three levels of indirection to answer "what is this folder called", which is a lot,
//! but every level is documented and none of them is a secret.
//!
//! Everything here treats its input as hostile: a damaged file will hand over offsets
//! that point past the end of a block, lengths that overlap, and heap ids that address
//! blocks which are not there. Each of those is an error naming what was wrong, never a
//! panic and never a silent empty answer.

use crate::ndb::{u16le, u32le, Node, Pst};
use std::collections::BTreeMap;

/// Marks the start of a heap. First byte of any heap-on-node.
const HN_SIGNATURE: u8 = 0xEC;
/// Client signature saying "this heap holds a property context".
pub const CLIENT_SIG_PC: u8 = 0xBC;
/// ...and a table context.
pub const CLIENT_SIG_TC: u8 = 0x7C;
/// First byte of a BTHHEADER.
const BTH_SIGNATURE: u8 = 0xB5;
/// First byte of a TCINFO.
const TC_SIGNATURE: u8 = 0x7C;

// The handful of properties needed to draw a folder tree.
pub const PID_DISPLAY_NAME: u16 = 0x3001;
pub const PID_CONTENT_COUNT: u16 = 0x3602;
pub const PID_UNREAD_COUNT: u16 = 0x3603;

// ...and for listing messages.
pub const PID_SUBJECT: u16 = 0x0037;
pub const PID_SENDER_NAME: u16 = 0x0C1A;
pub const PID_DELIVERY_TIME: u16 = 0x0E06;
pub const PID_SUBMIT_TIME: u16 = 0x0039;
pub const PID_MESSAGE_SIZE: u16 = 0x0E08;

// ...and for writing one back out as a mail message.
/// The original internet headers, exactly as the message arrived.
pub const PID_TRANSPORT_HEADERS: u16 = 0x007D;
pub const PID_SENDER_EMAIL: u16 = 0x0C1F;
pub const PID_DISPLAY_TO: u16 = 0x0E04;
pub const PID_DISPLAY_CC: u16 = 0x0E03;
pub const PID_BODY: u16 = 0x1000;
pub const PID_BODY_HTML: u16 = 0x1013;
pub const PID_INTERNET_CODEPAGE: u16 = 0x3FDE;
pub const PID_INTERNET_MSG_ID: u16 = 0x1035;
pub const PID_ATTACH_DATA: u16 = 0x3701;
pub const PID_ATTACH_FILENAME: u16 = 0x3704;
pub const PID_ATTACH_LONG_FILENAME: u16 = 0x3707;
pub const PID_ATTACH_METHOD: u16 = 0x3705;
pub const PID_ATTACH_MIME_TAG: u16 = 0x370E;
/// NID of the root folder. Fixed by the specification, the same in every file.
pub const NID_ROOT_FOLDER: u32 = 0x122;
/// A folder's tables share its node id with the type bits swapped: the folders under it,
/// and the messages in it.
pub const NID_TYPE_HIERARCHY_TABLE: u32 = 0x0D;
pub const NID_TYPE_CONTENTS_TABLE: u32 = 0x0E;
/// The recipient table hangs off a message as a subnode.
pub const NID_TYPE_RECIPIENT_TABLE: u32 = 0x12;
pub const PID_RECIPIENT_TYPE: u16 = 0x0C15;
pub const PID_EMAIL_ADDRESS: u16 = 0x3003;
pub const PID_SMTP_ADDRESS: u16 = 0x39FE;
/// NID type of an attachment, which only ever appears as a subnode of its message.
pub const NID_TYPE_ATTACHMENT: u32 = 0x05;
/// PidTagAttachMethod saying the bytes are right here in PidTagAttachDataBinary.
pub const ATTACH_BY_VALUE: i64 = 1;

/// A heap spread over a node's blocks.
pub struct Heap {
    blocks: Vec<Vec<u8>>,
    /// What the heap contains: 0xBC for properties, 0x7C for a table.
    pub client_sig: u8,
    /// Heap id of whatever the client considers the root of its structure.
    pub user_root: u32,
}

impl Heap {
    pub fn new(blocks: Vec<Vec<u8>>) -> Result<Heap, String> {
        let b0 = blocks.first().ok_or("node has no data blocks")?;
        if b0.len() < 12 {
            return Err(format!(
                "first block is {} bytes, too short for a heap header",
                b0.len()
            ));
        }
        if b0[2] != HN_SIGNATURE {
            return Err(format!(
                "not a heap: signature byte is 0x{:02X}, expected 0x{HN_SIGNATURE:02X}",
                b0[2]
            ));
        }
        Ok(Heap {
            client_sig: b0[3],
            user_root: u32le(b0, 4),
            blocks,
        })
    }

    /// The bytes of one heap allocation.
    ///
    /// A heap id packs which block, and which allocation within it. Every block — the
    /// first with its full header, the later ones with a short one — begins with the
    /// offset of its own allocation map, so finding an item is the same two steps
    /// wherever it lives.
    pub fn item(&self, hid: u32) -> Result<&[u8], String> {
        if hid == 0 {
            return Ok(&[]);
        }
        if hid & 0x1F != 0 {
            return Err(format!("0x{hid:08X} is a subnode id, not a heap id"));
        }
        let index = ((hid >> 5) & 0x7FF) as usize;
        let block = (hid >> 16) as usize;
        if index == 0 {
            return Err(format!(
                "heap id 0x{hid:08X} has allocation index 0, which cannot exist"
            ));
        }
        let b = self.blocks.get(block).ok_or_else(|| {
            format!(
                "heap id 0x{hid:08X} wants block {block}, node has {}",
                self.blocks.len()
            )
        })?;

        // HNPAGEMAP: cAlloc, cFree, then cAlloc+1 offsets. Item n runs from offset n-1
        // to offset n.
        let map = u16le_at(b, 0)? as usize;
        let count = u16le_at(b, map)? as usize;
        if index > count {
            return Err(format!(
                "heap id 0x{hid:08X} wants allocation {index} of {count} in block {block}"
            ));
        }
        let start = u16le_at(b, map + 4 + (index - 1) * 2)? as usize;
        let end = u16le_at(b, map + 4 + index * 2)? as usize;
        if start > end || end > b.len() {
            return Err(format!(
                "heap id 0x{hid:08X} spans {start}..{end} in a {}-byte block",
                b.len()
            ));
        }
        Ok(&b[start..end])
    }
}

fn u16le_at(b: &[u8], o: usize) -> Result<u16, String> {
    if o + 2 > b.len() {
        return Err(format!(
            "offset {o} is past the end of a {}-byte block",
            b.len()
        ));
    }
    Ok(u16le(b, o))
}

/// Walk a B-tree-on-heap and hand back every leaf record.
///
/// Depth is bounded by the header's own level count, so a damaged tree cannot be made to
/// recurse forever.
fn bth_records(heap: &Heap) -> Result<(usize, Vec<Vec<u8>>), String> {
    let h = heap.item(heap.user_root)?;
    if h.len() < 8 {
        return Err(format!("B-tree header is {} bytes, expected 8", h.len()));
    }
    if h[0] != BTH_SIGNATURE {
        return Err(format!(
            "not a B-tree on heap: type byte is 0x{:02X}, expected 0x{BTH_SIGNATURE:02X}",
            h[0]
        ));
    }
    let (key, ent, levels, root) = (h[1] as usize, h[2] as usize, h[3], u32le(h, 4));
    if key == 0 || ent == 0 {
        return Err("B-tree declares zero-width keys or entries".into());
    }

    let mut out = Vec::new();
    let mut queue = vec![(root, levels)];
    while let Some((hid, level)) = queue.pop() {
        if hid == 0 {
            continue;
        }
        let data = heap.item(hid)?;
        // Leaves are key+value; everything above is key plus the id of the next level.
        let width = if level == 0 { key + ent } else { key + 4 };
        for rec in data.chunks_exact(width) {
            if level == 0 {
                out.push(rec.to_vec());
            } else {
                queue.push((u32le(rec, key), level - 1));
            }
        }
    }
    Ok((key, out))
}

/// One property value, resolved.
///
/// Every variant carries its payload even where nothing reads it yet, so that a property
/// this layer cannot decode is still *present* rather than quietly dropped. Losing data
/// silently is the one thing a recovery tool must never do.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Bytes(Vec<u8>),
    /// 100-nanosecond ticks since 1601, which is how Windows keeps time.
    Time(u64),
    /// The value said it lived in a subnode that the node's subnode tree does not list.
    /// Recorded rather than dropped, because in a damaged file this is the interesting
    /// case: the property survived and its contents did not.
    MissingSubnode(u32),
    /// A type this layer does not decode yet, kept so nothing is silently lost.
    Raw {
        ptype: u16,
        bytes: Vec<u8>,
    },
}

/// A property context: every property on a folder, message or attachment.
pub struct Pc {
    pub props: BTreeMap<u16, Value>,
    /// How many values were too big for the heap and had to be fetched from the node's
    /// subnode tree. Worth reporting: it is the difference between a node that is
    /// self-contained and one whose contents live somewhere else in the file.
    pub from_subnode: usize,
}

/// Fixed-size types of four bytes or less are stored inline, where a reference would
/// otherwise go. Everything else is a heap id, or a subnode id.
fn is_inline(ptype: u16) -> bool {
    matches!(ptype, 0x0000..=0x0004 | 0x000A | 0x000B)
}

/// Read every property on a node in the node B-tree.
pub fn read_node_pc(pst: &mut Pst, node: &Node) -> Result<Pc, String> {
    read_pc(pst, node.bid_data, node.bid_sub)
}

/// Read every property on anything that has a data block and a subnode tree.
///
/// Addressed by block id rather than by node, because attachments are subnodes and never
/// appear in the node B-tree at all.
///
/// Needs the file, not just the heap, because a value too big to fit the heap is stored
/// out in the subnode tree and has to be fetched from there.
pub fn read_pc(pst: &mut Pst, bid_data: u64, bid_sub: u64) -> Result<Pc, String> {
    let heap = Heap::new(pst.node_blocks(bid_data)?)?;
    if heap.client_sig != CLIENT_SIG_PC {
        return Err(format!(
            "heap holds client type 0x{:02X}, not a property context",
            heap.client_sig
        ));
    }
    let (key, records) = bth_records(&heap)?;
    if key != 2 {
        return Err(format!("property B-tree has {key}-byte keys, expected 2"));
    }

    // Only fetched if some property actually points out of the heap, which most do not.
    let mut subs = None;
    let mut from_subnode = 0;

    let mut props = BTreeMap::new();
    for r in records {
        if r.len() < 8 {
            continue;
        }
        let (id, ptype, raw) = (u16le(&r, 0), u16le(&r, 2), u32le(&r, 4));

        if is_inline(ptype) {
            props.insert(
                id,
                match ptype {
                    0x0002 => Value::Int(raw as u16 as i16 as i64),
                    0x0004 => Value::Float(f32::from_bits(raw) as f64),
                    0x000B => Value::Bool(raw & 1 != 0),
                    _ => Value::Int(raw as i32 as i64),
                },
            );
            continue;
        }

        // A heap id has zero in its low five bits; anything else is a subnode id.
        let bytes = if raw == 0 {
            Vec::new()
        } else if raw & 0x1F == 0 {
            heap.item(raw)?.to_vec()
        } else {
            if subs.is_none() {
                subs = Some(pst.subnodes(bid_sub)?);
            }
            match subs.as_ref().unwrap().get(&raw).copied() {
                Some(s) => {
                    from_subnode += 1;
                    pst.node_blocks(s.data)?.concat()
                }
                None => {
                    props.insert(id, Value::MissingSubnode(raw));
                    continue;
                }
            }
        };

        props.insert(id, decode(ptype, bytes));
    }
    Ok(Pc {
        props,
        from_subnode,
    })
}

fn decode(ptype: u16, b: Vec<u8>) -> Value {
    let eight = |b: &[u8]| -> Option<u64> {
        (b.len() >= 8).then(|| u64::from_le_bytes(b[..8].try_into().unwrap()))
    };
    match ptype {
        0x001F => Value::Str(utf16le(&b)),
        // PtypString8 is in the file's own code page, which is not recorded anywhere in
        // the file. Treated as Latin-1: right for western text, and never a panic.
        0x001E => Value::Str(b.iter().map(|&c| c as char).collect()),
        0x0040 => eight(&b)
            .map(Value::Time)
            .unwrap_or(Value::Raw { ptype, bytes: b }),
        0x0014 | 0x0006 => eight(&b)
            .map(|v| Value::Int(v as i64))
            .unwrap_or(Value::Raw { ptype, bytes: b }),
        0x0005 | 0x0007 => eight(&b)
            .map(|v| Value::Float(f64::from_bits(v)))
            .unwrap_or(Value::Raw { ptype, bytes: b }),
        0x0102 | 0x0048 => Value::Bytes(b),
        _ => Value::Raw { ptype, bytes: b },
    }
}

impl Pc {
    pub fn str(&self, id: u16) -> Option<&str> {
        match self.props.get(&id) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn int(&self, id: u16) -> Option<i64> {
        match self.props.get(&id) {
            Some(Value::Int(i)) => Some(*i),
            Some(Value::Bool(b)) => Some(*b as i64),
            _ => None,
        }
    }

    pub fn time(&self, id: u16) -> Option<u64> {
        match self.props.get(&id) {
            Some(Value::Time(t)) => Some(*t),
            _ => None,
        }
    }
}

/// Year, month, day, hour, minute, second, and day of the week, from a Windows FILETIME.
///
/// Done by hand rather than with a date crate: it is one well-known algorithm and the
/// alternative is a dependency for arithmetic on a number.
fn civil(ft: u64) -> (i64, i64, i64, i64, i64, i64, usize) {
    // FILETIME counts 100ns ticks from 1601; Unix counts seconds from 1970.
    let unix = ft as i64 / 10_000_000 - 11_644_473_600;
    let (days, secs) = (unix.div_euclid(86400), unix.rem_euclid(86400));

    // Hinnant's civil-from-days: shift the epoch to 1st March so leap day lands last.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);

    // 1970-01-01 was a Thursday, which is index 4 counting from Sunday.
    let dow = (days + 4).rem_euclid(7) as usize;
    (y, m, d, secs / 3600, secs / 60 % 60, secs % 60, dow)
}

/// A Windows FILETIME as `YYYY-MM-DD HH:MM`, for the listings.
pub fn filetime(ft: u64) -> String {
    if ft == 0 {
        return "                ".into();
    }
    let (y, m, d, hh, mm, ..) = civil(ft);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// A FILETIME in the form an mbox `From ` separator line uses.
pub fn asctime(ft: u64) -> String {
    const DAY: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d, hh, mm, ss, dow) = civil(ft);
    format!(
        "{} {} {d:2} {hh:02}:{mm:02}:{ss:02} {y}",
        DAY[dow],
        MON[(m - 1) as usize]
    )
}

/// A FILETIME as an RFC 5322 `Date:` header.
///
/// Always +0000: a PST records the instant, not the timezone it was displayed in, so
/// claiming any offset would be inventing information.
pub fn rfc5322_date(ft: u64) -> String {
    const DAY: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d, hh, mm, ss, dow) = civil(ft);
    format!(
        "{}, {d} {} {y} {hh:02}:{mm:02}:{ss:02} +0000",
        DAY[dow],
        MON[(m - 1) as usize]
    )
}

/// Outlook prefixes a subject with U+0001 and a length byte when it has a "RE:"-style
/// prefix. Both are control characters that would otherwise be printed.
pub fn clean_subject(s: &str) -> &str {
    s.strip_prefix('\u{1}').map_or(s, |rest| {
        let mut c = rest.chars();
        c.next();
        c.as_str()
    })
}

/// One column of a table context.
pub struct Column {
    pub prop: u16,
    pub ptype: u16,
    /// Where the cell sits in a row, and how wide it is.
    ib: usize,
    cb: usize,
    /// Which bit of the row's cell-existence bitmap says whether this cell is set.
    bit: usize,
}

/// A table context: the rows of a hierarchy table, contents table or recipient table.
pub struct Tc {
    pub rows: Vec<Row>,
}

pub struct Row {
    /// For a folder's tables this is the NID of the child folder or message; for a
    /// recipient table it is the row number.
    pub id: u32,
    pub props: BTreeMap<u16, Value>,
}

impl Row {
    pub fn str(&self, id: u16) -> Option<&str> {
        match self.props.get(&id) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }
    pub fn int(&self, id: u16) -> Option<i64> {
        match self.props.get(&id) {
            Some(Value::Int(i)) => Some(*i),
            _ => None,
        }
    }
}

/// Read a table context: a grid of properties, one row per thing the table lists.
///
/// Unlike a property context, a table stores every fixed-size value inline in the row
/// however wide it is — a timestamp is eight bytes sitting in the row itself, not a
/// reference to the heap. Only genuinely variable things go elsewhere.
pub fn read_tc(pst: &mut Pst, bid_data: u64, bid_sub: u64) -> Result<Tc, String> {
    let heap = Heap::new(pst.node_blocks(bid_data)?)?;
    if heap.client_sig != CLIENT_SIG_TC {
        return Err(format!(
            "heap holds client type 0x{:02X}, not a table context",
            heap.client_sig
        ));
    }
    let info = heap.item(heap.user_root)?;
    if info.len() < 22 {
        return Err(format!("table header is {} bytes, expected at least 22", info.len()));
    }
    if info[0] != TC_SIGNATURE {
        return Err(format!(
            "not a table context: type byte is 0x{:02X}, expected 0x{TC_SIGNATURE:02X}",
            info[0]
        ));
    }
    let ncols = info[1] as usize;
    // Ending offsets within a row for the 4-byte, 2-byte and 1-byte cell groups, and
    // then for the bitmap. The last is therefore the width of a whole row.
    let ceb_start = u16le(info, 6) as usize;
    let width = u16le(info, 8) as usize;
    let hnid_rows = u32le(info, 14);
    if info.len() < 22 + ncols * 8 {
        return Err(format!("table declares {ncols} columns that do not fit its header"));
    }
    if width == 0 || ceb_start > width {
        return Err(format!("table rows are {width} bytes with a bitmap at {ceb_start}"));
    }

    let mut columns = Vec::with_capacity(ncols);
    for i in 0..ncols {
        let d = &info[22 + i * 8..30 + i * 8];
        let tag = u32le(d, 0);
        columns.push(Column {
            prop: (tag >> 16) as u16,
            ptype: tag as u16,
            ib: u16le(d, 4) as usize,
            cb: d[6] as usize,
            bit: d[7] as usize,
        });
    }

    // The rows live in the heap for a small table, or out in a subnode for a big one.
    let blocks = if hnid_rows == 0 {
        Vec::new()
    } else if hnid_rows & 0x1F == 0 {
        vec![heap.item(hnid_rows)?.to_vec()]
    } else {
        match pst.subnodes(bid_sub)?.get(&hnid_rows).copied() {
            Some(s) => pst.node_blocks(s.data)?,
            None => return Err(format!("table rows are in subnode 0x{hnid_rows:08X}, which is not there")),
        }
    };

    // A row never straddles two blocks: each block holds as many whole rows as fit and
    // the remainder is padding. Concatenating the blocks first would shift every row
    // after the first block by that padding and quietly produce garbage.
    let mut subs = None;
    let mut rows = Vec::new();
    for b in &blocks {
        for r in b.chunks_exact(width).take(b.len() / width) {
            let mut props = BTreeMap::new();
            for c in &columns {
                if c.ib + c.cb > width || c.bit / 8 >= width - ceb_start {
                    continue;
                }
                let ceb = &r[ceb_start..width];
                if ceb[c.bit / 8] & (0x80 >> (c.bit % 8)) == 0 {
                    continue; // the cell is not set on this row
                }
                let cell = &r[c.ib..c.ib + c.cb];
                let v = if is_variable(c.ptype) {
                    let hnid = u32le(cell, 0);
                    let bytes = if hnid == 0 {
                        Vec::new()
                    } else if hnid & 0x1F == 0 {
                        heap.item(hnid).unwrap_or(&[]).to_vec()
                    } else {
                        if subs.is_none() {
                            subs = Some(pst.subnodes(bid_sub).unwrap_or_default());
                        }
                        match subs.as_ref().unwrap().get(&hnid).copied() {
                            Some(s) => pst.node_blocks(s.data).unwrap_or_default().concat(),
                            None => {
                                props.insert(c.prop, Value::MissingSubnode(hnid));
                                continue;
                            }
                        }
                    };
                    decode(c.ptype, bytes)
                } else {
                    decode_inline(c.ptype, cell)
                };
                props.insert(c.prop, v);
            }
            rows.push(Row { id: u32le(r, 0), props });
        }
    }
    Ok(Tc { rows })
}

/// One addressee of a message.
pub struct Recipient {
    /// 1 for To, 2 for Cc, 3 for Bcc.
    pub kind: i64,
    pub name: String,
    pub email: String,
}

/// The real addressees of a message, from its recipient table.
///
/// This is what the display-name properties cannot give: `PidTagDisplayTo` holds the
/// names Outlook showed, whereas this holds the addresses mail was actually sent to.
pub fn read_recipients(pst: &mut Pst, bid_sub: u64) -> Vec<Recipient> {
    let Ok(subs) = pst.subnodes(bid_sub) else { return Vec::new() };
    let Some((_, sub)) = subs
        .iter()
        .find(|(nid, _)| *nid & 0x1F == NID_TYPE_RECIPIENT_TABLE)
        .map(|(n, s)| (*n, *s))
    else {
        return Vec::new();
    };
    let Ok(tc) = read_tc(pst, sub.data, sub.sub) else { return Vec::new() };

    tc.rows
        .iter()
        .map(|r| Recipient {
            kind: r.int(PID_RECIPIENT_TYPE).unwrap_or(1),
            name: r.str(PID_DISPLAY_NAME).unwrap_or("").to_string(),
            // The SMTP address is the one worth having; the other is often an X.500
            // path that means nothing outside the Exchange organisation it came from.
            email: r
                .str(PID_SMTP_ADDRESS)
                .or(r.str(PID_EMAIL_ADDRESS))
                .unwrap_or("")
                .to_string(),
        })
        .collect()
}

/// Whether a property type is stored somewhere else and referenced, rather than sitting
/// in the row. Everything of a fixed size lives in the row, however wide.
fn is_variable(ptype: u16) -> bool {
    matches!(ptype, 0x000D | 0x001E | 0x001F | 0x0102) || ptype & 0x1000 != 0
}

fn decode_inline(ptype: u16, b: &[u8]) -> Value {
    let n = |w: usize| -> Option<u64> {
        (b.len() >= w).then(|| b[..w].iter().rev().fold(0u64, |a, &x| (a << 8) | x as u64))
    };
    match ptype {
        0x0002 => Value::Int(n(2).unwrap_or(0) as u16 as i16 as i64),
        0x0003 | 0x000A => Value::Int(n(4).unwrap_or(0) as u32 as i32 as i64),
        0x000B => Value::Bool(n(1).unwrap_or(0) & 1 != 0),
        0x0004 => Value::Float(f32::from_bits(n(4).unwrap_or(0) as u32) as f64),
        0x0005 | 0x0007 => Value::Float(f64::from_bits(n(8).unwrap_or(0))),
        0x0006 | 0x0014 => Value::Int(n(8).unwrap_or(0) as i64),
        0x0040 => Value::Time(n(8).unwrap_or(0)),
        _ => Value::Raw { ptype, bytes: b.to_vec() },
    }
}

/// Outlook stores strings as UTF-16LE. A damaged file can leave an odd trailing byte or
/// an unpaired surrogate, so this drops those rather than refusing the whole name.
fn utf16le(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndb::Pst;
    use std::collections::BTreeSet;

    /// Read every folder in a fixture, end to end: node B-tree, block index, block
    /// decode, heap, B-tree on heap, properties. Any failure anywhere surfaces here.
    fn folder_names(file: &str) -> Vec<String> {
        let path = format!("tests/data/{file}");
        if !std::path::Path::new(&path).exists() {
            eprintln!("skipping {file}: run tests/fetch-fixtures.ps1");
            return Vec::new();
        }
        let mut pst = Pst::open(&path).expect("fixture should open");
        let nodes = pst.nodes();
        let mut names = Vec::new();
        for n in nodes.iter().filter(|n| n.nid_type() == 0x02) {
            let pc = read_node_pc(&mut pst, n)
                .unwrap_or_else(|e| panic!("{file} folder 0x{:X}: {e}", n.nid));
            if let Some(s) = pc.str(PID_DISPLAY_NAME) {
                names.push(s.to_string());
            }
        }
        assert!(!names.is_empty(), "{file}: no folder names at all");
        names
    }

    /// Messages are where properties outgrow the node's own heap, so this is the check
    /// that the subnode tree is actually walked and not merely present.
    #[test]
    fn reads_properties_out_of_the_subnode_tree() {
        let path = "tests/data/example-2013.ost";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let mut pst = Pst::open(path).unwrap();
        let nodes = pst.nodes();
        let mut fetched = 0;
        for n in nodes.iter().filter(|n| n.nid_type() == 0x04) {
            let pc = read_node_pc(&mut pst, n).unwrap();
            fetched += pc.from_subnode;
            // A value that says it lives in a subnode, in a node whose subnode tree does
            // not list it, means the walk is wrong or the file is damaged. Neither is
            // true of these fixtures, so it must not happen.
            assert!(
                !pc.props
                    .values()
                    .any(|v| matches!(v, Value::MissingSubnode(_))),
                "message 0x{:X} points at a subnode that is not in its subnode tree",
                n.nid
            );
        }
        assert!(fetched > 0, "not one property came out of a subnode tree");
    }

    /// Every message in a fixture, through the subnode tree where the value needs it.
    #[test]
    fn reads_messages() {
        for file in ["passworded.pst", "dist-list.pst", "example-2013.ost"] {
            let path = format!("tests/data/{file}");
            if !std::path::Path::new(&path).exists() {
                continue;
            }
            let mut pst = Pst::open(&path).unwrap();
            let nodes = pst.nodes();
            let (mut seen, mut timed) = (0, 0);
            for n in nodes.iter().filter(|n| n.nid_type() == 0x04) {
                let pc = read_node_pc(&mut pst, n)
                    .unwrap_or_else(|e| panic!("{file} message 0x{:X}: {e}", n.nid));
                assert!(
                    pc.props.contains_key(&PID_SUBJECT),
                    "{file} message 0x{:X} has no subject property",
                    n.nid
                );
                // Not every message node is mail. Outlook keeps internal objects like
                // LocalFreebusy in here too, and those have no delivery or submit time.
                if pc
                    .time(PID_DELIVERY_TIME)
                    .or(pc.time(PID_SUBMIT_TIME))
                    .is_some()
                {
                    timed += 1;
                }
                seen += 1;
            }
            assert!(seen > 0, "{file}: no messages found");
            assert!(
                timed > 0,
                "{file}: not one of {seen} messages had a readable timestamp"
            );
        }
    }

    #[test]
    fn formats_a_filetime() {
        // 2024-01-01T00:00:00Z is 13348540800 seconds after 1601.
        assert_eq!(filetime(133_485_408_000_000_000), "2024-01-01 00:00");
        // One tick before the next day, to catch an off-by-one in the day split.
        assert_eq!(filetime(133_486_271_999_999_999), "2024-01-01 23:59");
        assert_eq!(filetime(0).trim(), "");
    }

    #[test]
    fn strips_the_subject_prefix_marker() {
        assert_eq!(clean_subject("\u{1}\u{5}RE: hello"), "RE: hello");
        assert_eq!(clean_subject("plain"), "plain");
        assert_eq!(clean_subject(""), "");
    }

    /// The whole point, in one test: a "password-protected" PST gives up its folder
    /// names without anything ever asking for the password.
    #[test]
    fn reads_folder_names_from_a_password_protected_pst() {
        let names = folder_names("passworded.pst");
        if names.is_empty() {
            return;
        }
        for expected in ["Inbox", "Deleted Items", "Sent Items", "Calendar"] {
            assert!(
                names.iter().any(|n| n == expected),
                "no {expected:?} in {names:?}"
            );
        }
    }

    /// Exercises the large-page layout and the zlib-compressed blocks with it.
    #[test]
    fn reads_folder_names_from_an_ost() {
        let names = folder_names("example-2013.ost");
        if names.is_empty() {
            return;
        }
        for expected in ["Inbox", "Outbox", "Organization Forms"] {
            assert!(
                names.iter().any(|n| n == expected),
                "no {expected:?} in {names:?}"
            );
        }
    }

    #[test]
    fn reads_folder_names_from_a_plain_pst() {
        let names = folder_names("dist-list.pst");
        if names.is_empty() {
            return;
        }
        assert!(names.iter().any(|n| n == "Inbox"), "no Inbox in {names:?}");
    }

    /// A folder's own hierarchy table must name the same children as the node B-tree's
    /// parent pointers. Two independent records of the same fact, which is exactly why
    /// having both is worth the work — where they disagree, the file is damaged.
    #[test]
    fn hierarchy_tables_agree_with_the_parent_pointers() {
        for file in ["dist-list.pst", "example-2013.ost"] {
            let path = format!("tests/data/{file}");
            if !std::path::Path::new(&path).exists() {
                continue;
            }
            let mut pst = Pst::open(&path).unwrap();
            let nodes = pst.nodes();
            let by_nid: BTreeMap<u32, crate::ndb::Node> =
                nodes.iter().map(|n| (n.nid, *n)).collect();

            let mut checked = 0;
            for f in nodes.iter().filter(|n| n.nid_type() == 0x02) {
                let table_nid = (f.nid & !0x1F) | NID_TYPE_HIERARCHY_TABLE;
                let Some(t) = by_nid.get(&table_nid) else { continue };
                let tc = read_tc(&mut pst, t.bid_data, t.bid_sub)
                    .unwrap_or_else(|e| panic!("{file} hierarchy table 0x{table_nid:X}: {e}"));

                let from_table: BTreeSet<u32> = tc.rows.iter().map(|r| r.id).collect();
                // Search folders are folders too as far as a hierarchy table is
                // concerned, and the root folder is its own parent - so neither the type
                // filter nor the self-reference can be left out of the comparison.
                let from_pointers: BTreeSet<u32> = nodes
                    .iter()
                    .filter(|n| {
                        matches!(n.nid_type(), 0x02 | 0x03)
                            && n.nid_parent == f.nid
                            && n.nid != f.nid
                    })
                    .map(|n| n.nid)
                    .collect();
                assert_eq!(
                    from_table, from_pointers,
                    "{file} folder 0x{:X}: table and parent pointers disagree",
                    f.nid
                );
                checked += 1;
            }
            assert!(checked > 5, "{file}: only {checked} hierarchy tables found");
        }
    }

    /// Contents tables name the messages in a folder, and carry a usable subject.
    #[test]
    fn contents_tables_list_the_messages() {
        let path = "tests/data/example-2013.ost";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let mut pst = Pst::open(path).unwrap();
        let nodes = pst.nodes();
        let by_nid: BTreeMap<u32, crate::ndb::Node> = nodes.iter().map(|n| (n.nid, *n)).collect();
        let mut listed = BTreeSet::new();

        for f in nodes.iter().filter(|n| n.nid_type() == 0x02) {
            let table_nid = (f.nid & !0x1F) | NID_TYPE_CONTENTS_TABLE;
            let Some(t) = by_nid.get(&table_nid) else { continue };
            let tc = read_tc(&mut pst, t.bid_data, t.bid_sub).unwrap();
            for r in &tc.rows {
                listed.insert(r.id);
            }
        }
        let real: BTreeSet<u32> =
            nodes.iter().filter(|n| n.nid_type() == 0x04).map(|n| n.nid).collect();
        assert_eq!(listed, real, "contents tables and the node list disagree");
    }

    #[test]
    fn rejects_a_heap_that_is_not_one() {
        let e = Heap::new(vec![vec![0u8; 32]])
            .err()
            .expect("zeroes are not a heap");
        assert!(e.contains("not a heap"), "unhelpful error: {e}");
    }

    #[test]
    fn rejects_a_heap_id_pointing_at_a_block_that_is_not_there() {
        let mut b = vec![0u8; 32];
        b[2] = HN_SIGNATURE;
        let heap = Heap::new(vec![b]).unwrap();
        // Block 9 of a one-block node.
        let e = heap.item(0x0009_0020).unwrap_err();
        assert!(e.contains("node has 1"), "unhelpful error: {e}");
    }

    #[test]
    fn odd_length_strings_do_not_panic() {
        assert_eq!(utf16le(&[0x41, 0x00, 0x42]), "A");
    }
}
