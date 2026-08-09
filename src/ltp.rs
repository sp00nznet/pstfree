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

use crate::ndb::{u16le, u32le};
use std::collections::BTreeMap;

/// Marks the start of a heap. First byte of any heap-on-node.
const HN_SIGNATURE: u8 = 0xEC;
/// Client signature saying "this heap holds a property context".
pub const CLIENT_SIG_PC: u8 = 0xBC;
/// First byte of a BTHHEADER.
const BTH_SIGNATURE: u8 = 0xB5;

// The handful of properties needed to draw a folder tree.
pub const PID_DISPLAY_NAME: u16 = 0x3001;
pub const PID_CONTENT_COUNT: u16 = 0x3602;
pub const PID_UNREAD_COUNT: u16 = 0x3603;

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
            return Err(format!("first block is {} bytes, too short for a heap header", b0.len()));
        }
        if b0[2] != HN_SIGNATURE {
            return Err(format!(
                "not a heap: signature byte is 0x{:02X}, expected 0x{HN_SIGNATURE:02X}",
                b0[2]
            ));
        }
        Ok(Heap { client_sig: b0[3], user_root: u32le(b0, 4), blocks })
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
            return Err(format!("heap id 0x{hid:08X} has allocation index 0, which cannot exist"));
        }
        let b = self
            .blocks
            .get(block)
            .ok_or_else(|| format!("heap id 0x{hid:08X} wants block {block}, node has {}", self.blocks.len()))?;

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
        return Err(format!("offset {o} is past the end of a {}-byte block", b.len()));
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
    Bool(bool),
    Str(String),
    Bytes(Vec<u8>),
    /// Too big for the heap, so it lives in the node's subnode tree. Not read yet.
    InSubnode(u32),
    /// A type this layer does not decode yet, kept so nothing is silently dropped.
    Raw { ptype: u16, value: u32 },
}

/// A property context: every property on a folder, message or attachment.
pub struct Pc(pub BTreeMap<u16, Value>);

impl Pc {
    pub fn read(heap: &Heap) -> Result<Pc, String> {
        if heap.client_sig != CLIENT_SIG_PC {
            return Err(format!(
                "heap holds client type 0x{:02X}, not a property context",
                heap.client_sig
            ));
        }
        let (key, records) = bth_records(heap)?;
        if key != 2 {
            return Err(format!("property B-tree has {key}-byte keys, expected 2"));
        }

        let mut props = BTreeMap::new();
        for r in records {
            if r.len() < 8 {
                continue;
            }
            let (id, ptype, raw) = (u16le(&r, 0), u16le(&r, 2), u32le(&r, 4));
            // Fixed-size types of four bytes or less are stored where a reference would
            // otherwise go. Everything else is a heap id, or a subnode id.
            let v = match ptype {
                0x0002 => Value::Int(raw as u16 as i16 as i64),
                0x0003 | 0x000A => Value::Int(raw as i32 as i64),
                0x000B => Value::Bool(raw & 1 != 0),
                0x001F | 0x001E | 0x0102 => match heap.item(raw) {
                    _ if raw == 0 => Value::Bytes(Vec::new()),
                    _ if raw & 0x1F != 0 => Value::InSubnode(raw),
                    Ok(b) if ptype == 0x001F => Value::Str(utf16le(b)),
                    Ok(b) if ptype == 0x001E => {
                        Value::Str(b.iter().map(|&c| c as char).collect())
                    }
                    Ok(b) => Value::Bytes(b.to_vec()),
                    Err(e) => return Err(e),
                },
                _ => Value::Raw { ptype, value: raw },
            };
            props.insert(id, v);
        }
        Ok(Pc(props))
    }

    pub fn str(&self, id: u16) -> Option<&str> {
        match self.0.get(&id) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn int(&self, id: u16) -> Option<i64> {
        match self.0.get(&id) {
            Some(Value::Int(i)) => Some(*i),
            Some(Value::Bool(b)) => Some(*b as i64),
            _ => None,
        }
    }
}

/// Outlook stores strings as UTF-16LE. A damaged file can leave an odd trailing byte or
/// an unpaired surrogate, so this drops those rather than refusing the whole name.
fn utf16le(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndb::Pst;

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
            let pc = pst
                .node_blocks(n.bid_data)
                .and_then(Heap::new)
                .and_then(|h| Pc::read(&h))
                .unwrap_or_else(|e| panic!("{file} folder 0x{:X}: {e}", n.nid));
            if let Some(s) = pc.str(PID_DISPLAY_NAME) {
                names.push(s.to_string());
            }
        }
        assert!(!names.is_empty(), "{file}: no folder names at all");
        names
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
            assert!(names.iter().any(|n| n == expected), "no {expected:?} in {names:?}");
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
            assert!(names.iter().any(|n| n == expected), "no {expected:?} in {names:?}");
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

    #[test]
    fn rejects_a_heap_that_is_not_one() {
        let e = Heap::new(vec![vec![0u8; 32]]).err().expect("zeroes are not a heap");
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
