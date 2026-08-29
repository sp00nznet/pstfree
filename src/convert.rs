//! Turn an Outlook 2013+ OST into a PST.
//!
//! This is the third thing the payware sells — "OST to PST Converter" — and the one
//! [`repair`](crate::repair) refused, correctly, because it is not a repair. A repair
//! copies blocks byte for byte and rebuilds only the index around them, which is why it
//! cannot corrupt a property. A conversion cannot do that: the 4K-page format an Outlook
//! 2013 OST uses stores its blocks compressed, up to 64KB each, and a PST's blocks are
//! plain and hold 8176 bytes at the very most. Every block has to be decoded, and some of
//! them have to be taken apart.
//!
//! So this rebuilds the file a level up. It walks the nodes, and for each one takes the
//! *data stream* behind it — whatever chain of blocks and XBLOCKs the OST used to hold it
//! — and lays that stream out again as a PST would: plain blocks, an XBLOCK over them
//! when there is more than one, an XXBLOCK when there are more than fit in an XBLOCK.
//! Subnode trees are rebuilt the same way, recursively, since an attachment is a subnode
//! and an embedded message is a subnode tree hanging off one.
//!
//! **Block boundaries are kept where they exist.** That is not tidiness. A heap-on-node —
//! which is how every property context and every table is stored — addresses its
//! allocations by *which block* they are in (MS-PST 2.3.1.1, the `hidBlockIndex` field of
//! an HID). Re-splitting a stream at different offsets would leave every one of those
//! pointing at the wrong place, and the file would parse and give back the wrong answers,
//! which is worse than failing. So a block is only ever divided when it is too big for
//! the target format to hold, and when that happens and the block is a heap, it is
//! reported rather than done quietly.
//!
//! Nothing is re-encoded: the output declares `NDB_CRYPT_NONE` and its blocks are written
//! plain. The encoding is keyless and protects nothing — that is the whole argument of
//! this project — so preserving it would be work in exchange for nothing.

use crate::ndb::{crc32, Node, Pst};
use crate::repair::{
    build_tree, pad, place_aligned, put16, put32, put64, sig, Rebuilt, AMAP_EVERY, AMAP_FIRST,
    BBTENTRY, BLOCK_ALIGN, ESSENTIAL, MAP_SLOTS, NBTENTRY, OFF_BID_NEXT_B, OFF_BID_NEXT_P, PAGE,
    PTYPE_BBT, PTYPE_NBT,
};
use std::io::{Seek, SeekFrom, Write};

/// The most a PST data block can hold: 8KB all in, less the 16-byte trailer.
/// MS-PST 2.2.2.8.
const MAX_DATA: usize = 8176;
/// An XBLOCK is a header and then 8 bytes an entry, in one block.
const X_ENTRIES: usize = (MAX_DATA - 8) / 8;
/// An SLBLOCK entry is a local id and two block ids, padded to 24.
const SL_ENTRIES: usize = (MAX_DATA - 8) / 24;
/// An SIBLOCK entry is a local id and the block below it.
const SI_ENTRIES: usize = (MAX_DATA - 8) / 16;

/// The PST client signature, 'SM'. An OST says 'SO' and is otherwise the same header.
const MAGIC_PST: u16 = 0x4D53;
/// `wVer` for the 512-byte-page Unicode format, and `wVerClient` to go with it. An
/// Outlook 2013 OST carries 36 and 12; both fixtures agree on what a PST carries.
const VER_UNICODE: u16 = 23;
const VER_CLIENT: u16 = 19;

/// How deep a subnode tree may nest before this gives up. An attachment is a subnode, an
/// embedded message is a subnode tree under one, and its attachments are subnodes of
/// that — real files go two or three deep. A hundred is a damaged file pointing at
/// itself, and the recursion here is the thing that would notice by overflowing a stack.
const MAX_NEST: u32 = 100;

/// A block id's low bits: bit 1 says the block is an index or a subnode tree rather than
/// data, which is what tells a reader not to try to decode it. MS-PST 2.2.2.2.
const BID_INTERNAL: u64 = 2;

/// The file being written, and the index of it built up as it goes.
///
/// Everything is written in one forward pass, so this never holds more than a single data
/// stream at a time. The header is the one thing written twice: a placeholder first,
/// because the B-tree roots it names do not exist until the end, and the real one after a
/// seek back to nought.
struct Out {
    f: std::io::BufWriter<std::fs::File>,
    name: String,
    /// Where the next block goes, before alignment and the map slots are accounted for.
    pos: u64,
    /// BBTENTRY leaves, in the order the blocks were written, which is bid order.
    bbt: Vec<(u64, Vec<u8>)>,
    /// The counter behind every id handed out. A BID's low two bits are flags, so the
    /// counter is the id shifted down by two, exactly as `bidNextB` in the header is.
    next: u64,
    problems: Vec<String>,
}

impl Out {
    fn new(path: &str, first_bid_index: u64) -> Result<Out, String> {
        let f = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
        let mut o = Out {
            f: std::io::BufWriter::with_capacity(1 << 20, f),
            name: path.to_string(),
            pos: 0,
            bbt: Vec::new(),
            next: first_bid_index,
            problems: Vec::new(),
        };
        // Zeroes over the header and the first section's map slots, so the file actually
        // contains them: the real header is written back over these at the end, once the
        // B-tree roots it has to name exist. Skipping the space rather than writing it
        // would leave the front of the file missing, which is the sort of thing that
        // still reads correctly by sweeping and is wrong all the same.
        let start = AMAP_FIRST + MAP_SLOTS * PAGE;
        pad(&mut o.f, &mut o.pos, start).map_err(|e| format!("{path}: {e}"))?;
        Ok(o)
    }

    fn io(&self, e: std::io::Error) -> String {
        format!("{}: {e}", self.name)
    }

    fn take_bid(&mut self, internal: bool) -> u64 {
        let bid = (self.next << 2) | if internal { BID_INTERNAL } else { 0 };
        self.next += 1;
        bid
    }

    /// Write one block and return the id it was given.
    ///
    /// Every block in the output goes through here, which is why there is exactly one
    /// place that knows a trailer's shape, and one place that knows a block may not sit
    /// where an allocation map page goes.
    fn block(&mut self, data: &[u8], internal: bool) -> Result<u64, String> {
        if data.len() > MAX_DATA {
            return Err(format!(
                "a {}-byte block was built, and {MAX_DATA} is the most a PST can hold — \
                 this is a bug in the converter, not in the file",
                data.len()
            ));
        }
        let bid = self.take_bid(internal);
        let span = (data.len() as u64 + 16).div_ceil(BLOCK_ALIGN) * BLOCK_ALIGN;
        let at = place_aligned(self.pos, span, BLOCK_ALIGN);

        let mut buf = vec![0u8; span as usize];
        buf[..data.len()].copy_from_slice(data);
        // BLOCKTRAILER: the length of the data, the signature over id and position, the
        // checksum over the data as written, and the id. MS-PST 2.2.2.8.1.
        let t = span as usize - 16;
        put16(&mut buf, t, data.len() as u16);
        put16(&mut buf, t + 2, sig(at, bid));
        put32(&mut buf, t + 4, crc32(data));
        put64(&mut buf, t + 8, bid);

        pad(&mut self.f, &mut self.pos, at).map_err(|e| self.io(e))?;
        self.f.write_all(&buf).map_err(|e| self.io(e))?;
        self.pos = at + span;

        // BBTENTRY: the block's id and offset, then its length and reference count.
        let mut e = vec![0u8; BBTENTRY];
        put64(&mut e, 0, bid);
        put64(&mut e, 8, at);
        put16(&mut e, 16, data.len() as u16);
        // Everything written here is named by exactly one parent, and a singly-referenced
        // block counts 2. No reader checks it; it is written honestly anyway.
        put16(&mut e, 18, 2);
        self.bbt.push((bid, e));
        Ok(bid)
    }

    /// Lay a node's data stream out again, and return the id that now names all of it.
    ///
    /// The stream comes back from the OST already decoded and inflated, as the list of
    /// blocks it was stored in. That list is kept: see the note at the top about heaps
    /// addressing themselves by block number.
    fn stream(&mut self, pst: &mut Pst, src: u64, what: &str) -> Result<u64, String> {
        if src == 0 {
            return Ok(0);
        }
        let mut parts: Vec<(u64, u64)> = Vec::new(); // (bid, bytes it holds)
        for b in pst.node_blocks(src)? {
            if b.len() <= MAX_DATA {
                parts.push((self.block(&b, false)?, b.len() as u64));
                continue;
            }
            // Too big for the target format, so it has to come apart. Fine for a message
            // body or an attachment, which are addressed by offset across the whole
            // stream; not fine for a heap, whose contents point at each other by block.
            if b.len() > 2 && b[2] == 0xEC {
                self.problems.push(format!(
                    "{what}: a {}-byte heap block had to be divided to fit a PST's 8176-byte \
                     limit. Heaps address themselves by block number, so anything stored in \
                     that one may now read back wrong.",
                    b.len()
                ));
            }
            for chunk in b.chunks(MAX_DATA) {
                parts.push((self.block(chunk, false)?, chunk.len() as u64));
            }
        }
        match parts.len() {
            0 => Ok(0),
            1 => Ok(parts[0].0),
            _ => self.xblock(&parts),
        }
    }

    /// An XBLOCK over a list of data blocks, or an XXBLOCK over XBLOCKs when one is not
    /// enough. MS-PST 2.2.2.8.3.2.
    fn xblock(&mut self, parts: &[(u64, u64)]) -> Result<u64, String> {
        if parts.len() > X_ENTRIES {
            let mut level1 = Vec::new();
            for group in parts.chunks(X_ENTRIES) {
                let bytes = group.iter().map(|(_, n)| n).sum();
                level1.push((self.xblock(group)?, bytes));
            }
            return self.array(&level1, 2);
        }
        self.array(parts, 1)
    }

    /// The body both levels share: a type, a level, a count, the bytes underneath, and
    /// then the ids.
    fn array(&mut self, parts: &[(u64, u64)], level: u8) -> Result<u64, String> {
        let mut body = vec![0u8; 8 + parts.len() * 8];
        body[0] = 0x01;
        body[1] = level;
        put16(&mut body, 2, parts.len() as u16);
        put32(
            &mut body,
            4,
            parts.iter().map(|(_, n)| n).sum::<u64>() as u32,
        );
        for (i, (bid, _)) in parts.iter().enumerate() {
            put64(&mut body, 8 + i * 8, *bid);
        }
        self.block(&body, true)
    }

    /// Rebuild a node's subnode tree: every subnode's own data and its own subnodes, then
    /// a fresh SLBLOCK naming them. MS-PST 2.2.2.8.3.3.
    fn subtree(&mut self, pst: &mut Pst, src: u64, nid: u32, depth: u32) -> Result<u64, String> {
        if src == 0 {
            return Ok(0);
        }
        if depth > MAX_NEST {
            return Err(format!(
                "subnode trees nest more than {MAX_NEST} deep under node 0x{nid:X} — \
                 the file points at itself"
            ));
        }
        // `subnodes` hands back the leaves with any SIBLOCK level above them already
        // flattened away, so what comes back is the whole set and the shape is this
        // code's to choose again.
        let mut subs: Vec<(u32, crate::ndb::Sub)> = pst.subnodes(src)?.into_iter().collect();
        subs.sort_by_key(|(k, _)| *k);

        let mut entries = Vec::new();
        for (sub_nid, s) in subs {
            let what = format!("node 0x{nid:X}, subnode 0x{sub_nid:X}");
            let data = self.stream(pst, s.data, &what)?;
            let below = self.subtree(pst, s.sub, nid, depth + 1)?;
            entries.push((sub_nid, data, below));
        }
        if entries.is_empty() {
            return Ok(0);
        }
        self.slblock(&entries)
    }

    fn slblock(&mut self, entries: &[(u32, u64, u64)]) -> Result<u64, String> {
        if entries.len() > SL_ENTRIES {
            let mut level1 = Vec::new();
            for group in entries.chunks(SL_ENTRIES) {
                level1.push((group[0].0, self.slblock(group)?));
            }
            return self.siblock(&level1);
        }
        let mut body = vec![0u8; 8 + entries.len() * 24];
        body[0] = 0x02;
        put16(&mut body, 2, entries.len() as u16);
        for (i, (nid, data, sub)) in entries.iter().enumerate() {
            let e = 8 + i * 24;
            put32(&mut body, e, *nid);
            put64(&mut body, e + 8, *data);
            put64(&mut body, e + 16, *sub);
        }
        self.block(&body, true)
    }

    fn siblock(&mut self, children: &[(u32, u64)]) -> Result<u64, String> {
        if children.len() > SI_ENTRIES {
            return Err(format!(
                "a node has more than {} subnodes, which needs a third level this does \
                 not write",
                SI_ENTRIES * SL_ENTRIES
            ));
        }
        let mut body = vec![0u8; 8 + children.len() * 16];
        body[0] = 0x02;
        body[1] = 1;
        put16(&mut body, 2, children.len() as u16);
        for (i, (nid, bid)) in children.iter().enumerate() {
            let e = 8 + i * 16;
            put32(&mut body, e, *nid);
            put64(&mut body, e + 8, *bid);
        }
        self.block(&body, true)
    }
}

/// Write an OST out as a PST, node by node.
pub fn convert(
    pst: &mut Pst,
    nodes: &[Node],
    out: &str,
    on: crate::Progress,
) -> Result<Rebuilt, String> {
    // Start the ids past everything the source used. Nothing carries over — every block
    // in the output is newly built — but a fresh range costs nothing and means an id in
    // the result can never be confused with an id in the file it came from.
    let first = (nodes
        .iter()
        .map(|n| n.bid_data.max(n.bid_sub))
        .max()
        .unwrap_or(0)
        >> 2)
        + 1;
    let mut o = Out::new(out, first)?;

    let mut nbt: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut dropped_nodes = 0;
    let mut keep = nodes.to_vec();
    keep.sort_by_key(|n| n.nid);

    for (i, n) in keep.iter().enumerate() {
        on(i as u64 + 1, keep.len() as u64);
        let what = format!("node 0x{:X}", n.nid);
        // A node whose data will not come out of the OST is left out and named, rather
        // than taking the whole conversion down with it. Its blocks may already be in the
        // output, unreferenced, which costs space and nothing else.
        let built = match o.stream(pst, n.bid_data, &what) {
            Ok(data) => o.subtree(pst, n.bid_sub, n.nid, 0).map(|sub| (data, sub)),
            Err(e) => Err(e),
        };
        match built {
            Ok((data, sub)) => {
                // NBTENTRY: the id, what holds its data, what holds its subnodes, and the
                // folder it belongs to.
                let mut e = vec![0u8; NBTENTRY];
                put64(&mut e, 0, n.nid as u64);
                put64(&mut e, 8, data);
                put64(&mut e, 16, sub);
                put32(&mut e, 24, n.nid_parent);
                nbt.push((n.nid as u64, e));
            }
            Err(e) => {
                dropped_nodes += 1;
                o.problems.push(format!("{what} was left out: {e}"));
            }
        }
    }
    if nbt.is_empty() {
        return Err("no node could be read out of this file — there is nothing to convert".into());
    }

    let have: std::collections::HashSet<u64> = nbt.iter().map(|(k, _)| *k).collect();
    let missing: Vec<String> = ESSENTIAL
        .iter()
        .filter(|(nid, _)| !have.contains(&(*nid as u64)))
        .map(|(nid, what)| format!("{what} (node 0x{nid:X})"))
        .collect();

    // The two trees go after the blocks, in the same layout a repair writes them in.
    let blocks = o.bbt.len();
    let node_count = nbt.len();
    let bbt = std::mem::take(&mut o.bbt);
    let mut next = o.pos;
    let mut next_bid = o.next << 2;
    let (nbt_bid, nbt_at, nbt_pages) =
        build_tree(nbt, NBTENTRY, PTYPE_NBT, &mut next, &mut next_bid);
    let (bbt_bid, bbt_at, bbt_pages) =
        build_tree(bbt, BBTENTRY, PTYPE_BBT, &mut next, &mut next_bid);

    for (at, page) in nbt_pages.iter().chain(bbt_pages.iter()) {
        pad(&mut o.f, &mut o.pos, *at).map_err(|e| o.io(e))?;
        o.f.write_all(page).map_err(|e| o.io(e))?;
        o.pos += page.len() as u64;
    }
    let eof = o.pos.div_ceil(BLOCK_ALIGN) * BLOCK_ALIGN;
    pad(&mut o.f, &mut o.pos, eof).map_err(|e| o.io(e))?;

    // Now the header, over the placeholder left at the front. Most of it is the source's
    // and none of this code's business; what changes is what the file now *is*.
    let mut header = pst.header_bytes()?;
    put16(&mut header, 8, MAGIC_PST); // wMagicClient: this is a PST now
    put16(&mut header, 10, VER_UNICODE); // wVer: 512-byte pages
    put16(&mut header, 12, VER_CLIENT); // wVerClient to match
    header[513] = 0; // bCryptMethod: the blocks went out plain
    put64(&mut header, 184, eof); // ibFileEof
    put64(&mut header, 216, nbt_bid); // ROOT.BREFNBT
    put64(&mut header, 224, nbt_at);
    put64(&mut header, 232, bbt_bid); // ROOT.BREFBBT
    put64(&mut header, 240, bbt_at);
    header[248] = 0; // fAMapValid = INVALID_AMAP
    put64(
        &mut header,
        192,
        AMAP_FIRST + (eof - AMAP_FIRST) / AMAP_EVERY * AMAP_EVERY,
    );
    put64(&mut header, 200, 0); // cbAMapFree
    put64(&mut header, 208, 0); // cbPMapFree
    put64(&mut header, OFF_BID_NEXT_P, next_bid);
    put64(&mut header, OFF_BID_NEXT_B, next_bid);
    // The partial checksum covers everything up to bidNextB, the full one takes that in
    // too, and neither range includes either checksum field. Computed before being
    // written, because the second range covers the first one's result.
    let partial = crc32(&header[8..8 + 471]);
    put32(&mut header, 4, partial);
    let full = crc32(&header[8..8 + 516]);
    put32(&mut header, 524, full);

    o.f.flush().map_err(|e| o.io(e))?;
    let mut f = o.f.into_inner().map_err(|e| format!("{out}: {e}"))?;
    f.seek(SeekFrom::Start(0))
        .map_err(|e| format!("{out}: {e}"))?;
    f.write_all(&header).map_err(|e| format!("{out}: {e}"))?;
    f.flush().map_err(|e| format!("{out}: {e}"))?;

    Ok(Rebuilt {
        nodes: node_count,
        blocks,
        dropped_blocks: 0,
        dropped_nodes,
        missing,
        bytes: eof,
        converted: true,
        problems: o.problems,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every block of a node's data stream, so the two files can be held against each
    /// other at the level the conversion actually promises: the same bytes, in the same
    /// blocks, in the same order.
    fn stream(p: &mut Pst, bid: u64) -> Vec<Vec<u8>> {
        if bid == 0 {
            return Vec::new();
        }
        p.node_blocks(bid).expect("a stream should read back")
    }

    /// Compare a subnode tree in the source against the one in the output: the same local
    /// ids, each with the same data and the same tree under it. The block ids differ by
    /// design — every one of them is new — so the comparison is on what they hold.
    fn same_subtree(a: &mut Pst, ab: u64, b: &mut Pst, bb: u64, what: &str) {
        let mut sa: Vec<_> = a
            .subnodes(ab)
            .expect("source subnodes")
            .into_iter()
            .collect();
        let mut sb: Vec<_> = b
            .subnodes(bb)
            .expect("output subnodes")
            .into_iter()
            .collect();
        sa.sort_by_key(|(k, _)| *k);
        sb.sort_by_key(|(k, _)| *k);
        assert_eq!(
            sa.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            sb.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            "{what}: different subnodes"
        );
        for ((nid, x), (_, y)) in sa.iter().zip(sb.iter()) {
            assert_eq!(
                stream(a, x.data),
                stream(b, y.data),
                "{what}: subnode 0x{nid:X} holds different bytes"
            );
            same_subtree(a, x.sub, b, y.sub, &format!("{what} / 0x{nid:X}"));
        }
    }

    /// The milestone, checked at the level it is written at: an OST goes in, a PST comes
    /// out, and every node in it holds the same bytes in the same blocks.
    ///
    /// The block boundaries are asserted, not just the contents. A heap addresses its own
    /// allocations by block number, so a stream that came back with the same bytes split
    /// differently would parse and answer wrongly — which is the failure this whole module
    /// is arranged to avoid, and the one a bytes-only comparison would sail past.
    #[test]
    fn an_ost_converts_to_a_pst_holding_the_same_mail() {
        let src = "tests/data/example-2013.ost";
        if !std::path::Path::new(src).exists() {
            eprintln!("skipping: run tests/fetch-fixtures.ps1");
            return;
        }
        let mut ost = Pst::open(src).expect("fixture should open");
        assert!(!ost.is_small_page(), "the fixture should be a 4K-page OST");
        let nodes = ost.nodes();

        let out = std::env::temp_dir().join("pstfree-converted.pst");
        let out = out.to_str().unwrap();
        let r = convert(&mut ost, &nodes, out, &mut |_, _| {}).expect("conversion should work");
        assert!(r.converted, "it should say it converted");
        assert!(r.missing.is_empty(), "converted without {:?}", r.missing);
        assert_eq!(r.dropped_nodes, 0, "a node was left behind");
        assert!(r.problems.is_empty(), "{:?}", r.problems);

        let mut pst = Pst::open(out).expect("the converted file should open");
        assert!(
            pst.is_small_page(),
            "the output should be a 512-byte-page file"
        );
        assert!(!pst.is_ost, "the output should say it is a PST");
        assert_eq!(pst.crypt, crate::ndb::Crypt::None, "blocks went out plain");

        // The maps go out invalid on purpose and a reader is right to say so; anything
        // else means a structure this wrote does not check out against its own checksum.
        let unexpected: Vec<_> = pst
            .warnings
            .iter()
            .filter(|w| !w.contains("allocation map"))
            .collect();
        assert!(unexpected.is_empty(), "{unexpected:?}");

        let after = pst.nodes();
        assert_eq!(after.len(), nodes.len(), "a node went missing");
        let by_nid: std::collections::BTreeMap<u32, Node> =
            after.iter().map(|n| (n.nid, *n)).collect();

        for n in &nodes {
            let m = by_nid[&n.nid];
            assert_eq!(
                m.nid_parent, n.nid_parent,
                "node 0x{:X} changed parent",
                n.nid
            );
            assert_eq!(
                stream(&mut ost, n.bid_data),
                stream(&mut pst, m.bid_data),
                "node 0x{:X} holds different bytes",
                n.nid
            );
            same_subtree(
                &mut ost,
                n.bid_sub,
                &mut pst,
                m.bid_sub,
                &format!("node 0x{:X}", n.nid),
            );
        }
        let _ = std::fs::remove_file(out);
    }

    /// The two levels no fixture reaches: a stream too long for one XBLOCK, and a node
    /// with more subnodes than one SLBLOCK holds.
    ///
    /// A 20MB attachment needs the first and nothing smaller does, so it is exactly the
    /// path that would be found by a user rather than by a test. Driven directly, because
    /// manufacturing an OST big enough to reach it is a bigger job than checking the
    /// arithmetic that decides it.
    #[test]
    fn the_second_level_of_each_tree_is_built_when_one_is_not_enough() {
        let path = std::env::temp_dir().join("pstfree-levels.bin");
        let path = path.to_str().unwrap();

        // Enough pieces for three XBLOCKs and an XXBLOCK over them.
        let mut o = Out::new(path, 1).expect("temp file");
        let parts: Vec<(u64, u64)> = (0..X_ENTRIES as u64 * 2 + 5)
            .map(|i| (i * 4, 100))
            .collect();
        let before = o.bbt.len();
        o.xblock(&parts).expect("an XXBLOCK should be built");
        let made = o.bbt.len() - before;
        assert_eq!(made, 4, "three XBLOCKs and one XXBLOCK");
        let last = o.bbt.len() - 1;
        let top = read_back(&mut o, path, last);
        assert_eq!(top[0], 0x01, "the top of a data tree is btype 0x01");
        assert_eq!(top[1], 2, "and level 2, which is what makes it an XXBLOCK");
        assert_eq!(
            u16::from_le_bytes([top[2], top[3]]),
            3,
            "over three XBLOCKs"
        );
        assert_eq!(
            u32::from_le_bytes([top[4], top[5], top[6], top[7]]) as u64,
            parts.len() as u64 * 100,
            "an XXBLOCK counts every byte underneath it"
        );

        // And enough subnodes for two SLBLOCKs under an SIBLOCK.
        let entries: Vec<(u32, u64, u64)> = (0..SL_ENTRIES as u32 + 1).map(|i| (i, 4, 0)).collect();
        let before = o.bbt.len();
        o.slblock(&entries).expect("an SIBLOCK should be built");
        let made = o.bbt.len() - before;
        assert_eq!(made, 3, "two SLBLOCKs and one SIBLOCK");
        let last = o.bbt.len() - 1;
        let top = read_back(&mut o, path, last);
        assert_eq!(top[0], 0x02, "a subnode tree is btype 0x02");
        assert_eq!(top[1], 1, "and level 1, which is what makes it an SIBLOCK");
        assert_eq!(u16::from_le_bytes([top[2], top[3]]), 2, "over two SLBLOCKs");

        let _ = std::fs::remove_file(path);
    }

    /// Pull one written block's data back off disk, by the offset and length its own BBT
    /// entry records.
    fn read_back(o: &mut Out, path: &str, which: usize) -> Vec<u8> {
        o.f.flush().expect("flush");
        let e = &o.bbt[which].1;
        let at = u64::from_le_bytes(e[8..16].try_into().unwrap());
        let cb = u16::from_le_bytes([e[16], e[17]]) as usize;
        let all = std::fs::read(path).expect("the temp file should read");
        all[at as usize..at as usize + cb].to_vec()
    }
}
