//! Reading an ANSI PST — Outlook 97 to 2002 — and converting one to a Unicode PST.
//!
//! **There is no ANSI fixture, because there is no public ANSI PST.** Every route was
//! checked when the corpus hunt was done for the damaged files: EDRM's Enron set, Digital
//! Corpora, every library's test data. What exists is Unicode. So this builds one, from
//! MS-PST 2.2.2.6 and 2.2.2.7, and reads it back.
//!
//! That is weaker evidence than a real file and is named as such in `docs/findings.md`.
//! It is not nothing, though, and it is stronger than it looks, because the file is
//! written from the *specification's* field order and read back by code that shares no
//! constants with what is written here. Every one of the ANSI differences is load-bearing
//! in a way that fails loudly if it is wrong:
//!
//! - Get the 12-byte trailer's field order wrong and the checksum field holds the block
//!   id, so every block fails its checksum.
//! - Get the entry widths wrong and the node ids come out as garbage, or the walk refuses
//!   the page for declaring entries too small.
//! - Get the subnode block's 4-byte header wrong — Unicode pads it to 8 — and the
//!   subnode tree reads one field to the left.
//! - Get the page footer's position wrong (496, not 488, because the trailer is shorter)
//!   and the entry count is read out of the last entry's bytes.
//!
//! None of those can pass by accident, which is the property that makes a synthetic
//! fixture worth having at all.

use pstfree::ndb::{crc32, Pst};

const PAGE: usize = 512;
const ALIGN: usize = 64;
const PTYPE_BBT: u8 = 0x80;
const PTYPE_NBT: u8 = 0x81;

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// MS-PST 5.5, the block signature: the offset XOR the id, folded in half. Never checked
/// by a reader, and written correctly anyway — a fixture that is wrong in a way nothing
/// looks at is a fixture that will mislead somebody later.
fn sig(ib: u32, bid: u32) -> u16 {
    let x = ib ^ bid;
    ((x >> 16) as u16) ^ (x as u16)
}

/// A file being built. Blocks first, then the two index pages, then the header over the
/// space left for it — the same order a real writer uses, for the same reason.
struct Build {
    buf: Vec<u8>,
    nbt: Vec<[u8; 16]>,
    bbt: Vec<[u8; 12]>,
}

impl Build {
    fn new() -> Build {
        Build {
            // The header occupies the first 512 bytes and is written last.
            buf: vec![0u8; PAGE],
            nbt: Vec::new(),
            bbt: Vec::new(),
        }
    }

    fn align_to(&mut self, n: usize) {
        while !self.buf.len().is_multiple_of(n) {
            self.buf.push(0);
        }
    }

    /// One block: its data, padding, then the ANSI BLOCKTRAILER — cb, wSig, **bid**,
    /// **dwCRC**, in that order. The Unicode one puts the checksum before the id.
    fn block(&mut self, bid: u32, data: &[u8]) {
        self.align_to(ALIGN);
        let at = self.buf.len() as u32;
        let total = (data.len() + 12).div_ceil(ALIGN) * ALIGN;

        let mut b = vec![0u8; total];
        b[..data.len()].copy_from_slice(data);
        let t = total - 12;
        put16(&mut b, t, data.len() as u16);
        put16(&mut b, t + 2, sig(at, bid));
        put32(&mut b, t + 4, bid);
        put32(&mut b, t + 8, crc32(data));
        self.buf.extend_from_slice(&b);

        // BBTENTRY, ANSI: a BREF, then cb and cRef. Twelve bytes, not twenty-four.
        let mut e = [0u8; 12];
        put32(&mut e, 0, bid);
        put32(&mut e, 4, at);
        put16(&mut e, 8, data.len() as u16);
        put16(&mut e, 10, 1);
        self.bbt.push(e);
    }

    /// NBTENTRY, ANSI: four 4-byte fields and no padding. Sixteen bytes, not thirty-two.
    fn node(&mut self, nid: u32, data: u32, sub: u32, parent: u32) {
        let mut e = [0u8; 16];
        put32(&mut e, 0, nid);
        put32(&mut e, 4, data);
        put32(&mut e, 8, sub);
        put32(&mut e, 12, parent);
        self.nbt.push(e);
    }

    /// A leaf BTPAGE. The entry array runs to 496 because the trailer is only 12 bytes,
    /// and the four count bytes sit between them.
    fn page(&mut self, ptype: u8, bid: u32, entries: &[&[u8]], width: usize) -> u32 {
        self.align_to(PAGE);
        let at = self.buf.len() as u32;
        let mut p = vec![0u8; PAGE];
        for (i, e) in entries.iter().enumerate() {
            p[i * width..i * width + e.len()].copy_from_slice(e);
        }
        p[496] = entries.len() as u8; // cEnt
        p[497] = (496 / width) as u8; // cEntMax
        p[498] = width as u8; // cbEnt
        p[499] = 0; // cLevel: a leaf
        p[500] = ptype;
        p[501] = ptype;
        put16(&mut p, 502, sig(at, bid));
        put32(&mut p, 504, bid); // the id comes first in ANSI
        let sum = crc32(&p[..500]);
        put32(&mut p, 508, sum);
        self.buf.extend_from_slice(&p);
        at
    }

    fn finish(mut self, crypt: u8) -> Vec<u8> {
        let nbt: Vec<Vec<u8>> = self.nbt.iter().map(|e| e.to_vec()).collect();
        let nbt: Vec<&[u8]> = nbt.iter().map(Vec::as_slice).collect();
        let nbt_at = self.page(PTYPE_NBT, 0x40, &nbt, 16);

        let bbt: Vec<Vec<u8>> = self.bbt.iter().map(|e| e.to_vec()).collect();
        let bbt: Vec<&[u8]> = bbt.iter().map(Vec::as_slice).collect();
        let bbt_at = self.page(PTYPE_BBT, 0x44, &bbt, 12);
        let eof = self.buf.len() as u32;

        // The ANSI HEADER, MS-PST 2.2.2.6: no bidUnused, no qwUnused, no dwAlign, and a
        // 40-byte ROOT, all of which is why every offset below is 16 short of the
        // Unicode one.
        let h = &mut self.buf[..PAGE];
        h[..4].copy_from_slice(b"!BDN");
        put16(h, 8, 0x4D53); // wMagicClient, "SM"
        put16(h, 10, 14); // wVer: ANSI
        put16(h, 12, 19); // wVerClient
        h[14] = 1; // bPlatformCreate
        h[15] = 1; // bPlatformAccess
        put32(h, 24, 0x100); // bidNextB
        put32(h, 28, 0x100); // bidNextP
        put32(h, 32, 7); // dwUnique
        put32(h, 168, eof); // ROOT.ibFileEof
        put32(h, 172, 0x4400); // ROOT.ibAMapLast
        put32(h, 184, 0x40); // ROOT.BREFNBT.bid
        put32(h, 188, nbt_at); // ROOT.BREFNBT.ib
        put32(h, 192, 0x44); // ROOT.BREFBBT.bid
        put32(h, 196, bbt_at); // ROOT.BREFBBT.ib
        h[200] = 1; // fAMapValid
        h[204..332].fill(0xFF); // rgbFM, deprecated, MUST be 0xFF
        h[332..460].fill(0xFF); // rgbFP, the same
        h[460] = 0x80; // bSentinel
        h[461] = crypt; // bCryptMethod

        // ANSI has dwCRCPartial and no dwCRCFull, over the same 471 bytes.
        let partial = crc32(&self.buf[8..8 + 471]);
        put32(&mut self.buf, 4, partial);
        self.buf
    }
}

/// Block ids advance in fours; bit 1 is fInternal and says "this one holds other ids".
const BID_STORE: u32 = 4;
const BID_ROOT: u32 = 8;
const BID_MSG: u32 = 12;
const BID_SUBDATA: u32 = 16;
const BID_PART_A: u32 = 20;
const BID_PART_B: u32 = 24;
const BID_SLBLOCK: u32 = 28 | 2;
const BID_XBLOCK: u32 = 32 | 2;

const STORE: &[u8] = b"message store node, such as it is";
const ROOT: &[u8] = b"root folder node";
const MSG: &[u8] = b"a message";
const SUBDATA: &[u8] = b"an attachment, out in the subnode tree";

fn sample(crypt: u8) -> Vec<u8> {
    let mut b = Build::new();

    b.block(BID_STORE, STORE);
    b.block(BID_ROOT, ROOT);
    b.block(BID_MSG, MSG);
    b.block(BID_SUBDATA, SUBDATA);
    b.block(BID_PART_A, &[0xA5; 300]);
    b.block(BID_PART_B, &[0x5C; 120]);

    // An SLBLOCK, ANSI: btype, cLevel, cEnt and **no padding**, then 12-byte entries.
    // Unicode puts four bytes of padding after the count, which is the whole difference.
    let mut sl = vec![0u8; 4 + 12];
    sl[0] = 0x02;
    sl[1] = 0;
    put16(&mut sl, 2, 1);
    put32(&mut sl, 4, 0x0000_0021); // the subnode's local id
    put32(&mut sl, 8, BID_SUBDATA);
    put32(&mut sl, 12, 0);
    b.block(BID_SLBLOCK, &sl);

    // An XBLOCK: btype, cLevel, cEnt, lcbTotal — 8 bytes in both formats — and then an
    // array of ids that is 4 bytes wide here and 8 in Unicode.
    let mut x = vec![0u8; 8 + 8];
    x[0] = 0x01;
    x[1] = 1;
    put16(&mut x, 2, 2);
    put32(&mut x, 4, 420); // lcbTotal
    put32(&mut x, 8, BID_PART_A);
    put32(&mut x, 12, BID_PART_B);
    b.block(BID_XBLOCK, &x);

    b.node(0x21, BID_STORE, 0, 0); // message store
    b.node(0x122, BID_ROOT, 0, 0); // root folder
    b.node(0x2024, BID_MSG, BID_SLBLOCK, 0x122); // a message, with a subnode tree
    b.node(0x8062, BID_XBLOCK, 0, 0x122); // a node whose data spans two blocks

    b.finish(crypt)
}

fn write(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(name);
    std::fs::write(&p, bytes).expect("temp file should write");
    p
}

#[test]
fn an_ansi_pst_opens_and_reads() {
    let p = write("pstfree-ansi.pst", &sample(0));
    let mut pst = Pst::open(p.to_str().unwrap()).expect("an ANSI PST should open");

    assert_eq!(pst.ver, 14, "the format version off the header");
    assert!(pst.is_ansi(), "it should know the ids are 32-bit");
    assert!(!pst.is_ost);
    assert!(
        !pst.is_small_page(),
        "ANSI has 512-byte pages and is still not the layout a repair copies"
    );
    assert!(
        pst.warnings.is_empty(),
        "a well-formed file should report nothing: {:?}",
        pst.warnings
    );

    // The node B-tree. A wrong entry width here gives nonsense ids rather than an error,
    // which is why these are checked one field at a time.
    let nodes = pst.nodes();
    assert_eq!(nodes.len(), 4);
    let msg = nodes.iter().find(|n| n.nid == 0x2024).expect("the message");
    assert_eq!(msg.bid_data, BID_MSG as u64);
    assert_eq!(msg.bid_sub, BID_SLBLOCK as u64);
    assert_eq!(msg.nid_parent, 0x122);
    assert_eq!(msg.nid_type(), 0x04, "low five bits say what it is");

    let blocks = pst.blocks();
    assert_eq!(blocks.len(), 8);
    let stored = blocks.iter().find(|b| b.bid == BID_MSG as u64).unwrap();
    assert_eq!(stored.cb as usize, MSG.len());

    // The 12-byte trailer, read the right way round. Getting the field order wrong makes
    // this fail as a checksum mismatch rather than as wrong data.
    assert_eq!(pst.block(BID_STORE as u64).unwrap(), STORE);
    assert_eq!(pst.block(BID_MSG as u64).unwrap(), MSG);
    assert!(
        pst.warnings.is_empty(),
        "no block should fail its own checksum: {:?}",
        pst.warnings
    );
}

#[test]
fn a_subnode_tree_reads_through_the_shorter_header() {
    let p = write("pstfree-ansi-sub.pst", &sample(0));
    let mut pst = Pst::open(p.to_str().unwrap()).unwrap();

    let subs = pst.subnodes(BID_SLBLOCK as u64).expect("the subnode tree");
    assert_eq!(subs.len(), 1);
    let s = subs.get(&0x21).expect("the subnode's local id");
    assert_eq!(s.data, BID_SUBDATA as u64);
    assert_eq!(s.sub, 0);
    assert_eq!(pst.block(s.data).unwrap(), SUBDATA);
}

#[test]
fn an_xblock_chains_blocks_through_a_narrow_id_array() {
    let p = write("pstfree-ansi-x.pst", &sample(0));
    let mut pst = Pst::open(p.to_str().unwrap()).unwrap();

    let parts = pst.node_blocks(BID_XBLOCK as u64).expect("the data stream");
    assert_eq!(parts.len(), 2, "two blocks, kept as two");
    assert_eq!(parts[0], vec![0xA5; 300]);
    assert_eq!(parts[1], vec![0x5C; 120]);
}

#[test]
fn recovery_works_on_an_ansi_file_too() {
    // The whole argument of the project is that a torn index is survivable. That has to
    // hold for this format as well, and it exercises a different set of offsets: the
    // sweep reads the page trailer itself, and carving reads the block trailers with no
    // index at all.
    let mut bytes = sample(0);
    let len = bytes.len();
    // Both B-tree roots, wiped — as dead as scanpst can describe.
    bytes[len - 2 * PAGE..].fill(0);
    let p = write("pstfree-ansi-torn.pst", &bytes);

    let mut pst = Pst::open(p.to_str().unwrap()).unwrap();
    assert!(
        pst.nodes().is_empty() && pst.blocks().is_empty(),
        "with the roots gone the header leads nowhere"
    );

    let carved = pst.carve();
    assert_eq!(carved.len(), 8, "every block found by its own trailer");
    pst.adopt(&carved);
    assert_eq!(pst.block(BID_MSG as u64).unwrap(), MSG);

    let swept = pst.scan();
    assert!(
        swept.nodes.is_empty(),
        "both index pages were destroyed, so there is nothing to sweep"
    );
}

#[test]
fn a_sweep_finds_ansi_nodes_when_only_the_header_is_gone() {
    let mut bytes = sample(0);
    // The header's roots alone, so the index pages themselves survive to be swept.
    bytes[184..200].fill(0);
    let p = write("pstfree-ansi-header.pst", &bytes);

    let mut pst = Pst::open(p.to_str().unwrap()).unwrap();
    let swept = pst.scan();
    assert_eq!(swept.nodes.len(), 4, "swept out of the surviving NBT page");
    assert_eq!(swept.blocks.len(), 8);
    assert!(
        swept.nodes.iter().any(|n| n.nid == 0x2024
            && n.bid_data == BID_MSG as u64
            && n.bid_sub == BID_SLBLOCK as u64),
        "the swept entries have to be read with ANSI widths too"
    );
}

#[test]
fn an_ansi_pst_converts_to_a_unicode_one() {
    let p = write("pstfree-ansi-src.pst", &sample(0));
    let out = std::env::temp_dir().join("pstfree-ansi-converted.pst");
    let _ = std::fs::remove_file(&out);

    let mut pst = Pst::open(p.to_str().unwrap()).unwrap();
    let nodes = pst.nodes();
    let blocks = pst.blocks();
    let r = pstfree::repair::rebuild(
        &mut pst,
        &nodes,
        &blocks,
        out.to_str().unwrap(),
        &mut |_, _| {},
    )
    .expect("the conversion should run");

    assert!(
        r.converted,
        "an ANSI source cannot be copied block for block"
    );
    assert_eq!(r.source, "an ANSI PST (Outlook 97-2002)");
    assert!(
        r.missing.is_empty(),
        "the store and the root folder both survived: {:?}",
        r.missing
    );

    // And the result is a Unicode PST, read back through its own header rather than by
    // sweeping — which is the check a tool that recovers from a broken index can
    // otherwise pass while writing one.
    let mut back = Pst::open(out.to_str().unwrap()).unwrap();
    assert_eq!(back.ver, 23);
    assert!(!back.is_ansi() && back.is_small_page());
    let got = back.nodes();
    assert_eq!(got.len(), nodes.len(), "every node came across");

    let msg = got.iter().find(|n| n.nid == 0x2024).expect("the message");
    assert_eq!(
        back.block(msg.bid_data).unwrap(),
        MSG,
        "the contents of a stream are the same bytes in both formats"
    );
    let subs = back.subnodes(msg.bid_sub).unwrap();
    assert_eq!(
        back.block(subs[&0x21].data).unwrap(),
        SUBDATA,
        "and so is what hangs off it"
    );

    // The two-block stream stays two blocks. Heap ids address an allocation by which
    // block it is in, so a stream silently re-split would parse and answer wrongly.
    let big = got.iter().find(|n| n.nid == 0x8062).unwrap();
    let parts = back.node_blocks(big.bid_data).unwrap();
    assert_eq!(parts.len(), 2, "block for block, not byte for byte");
    assert_eq!(parts[0], vec![0xA5; 300]);
    assert_eq!(parts[1], vec![0x5C; 120]);
}

#[test]
fn an_encoded_ansi_file_needs_no_password() {
    // Two things at once, both ANSI-specific and both silent when wrong.
    //
    // First: `bCryptMethod` is at offset 461 in an ANSI header and 513 in a Unicode one.
    // Read it from the wrong place and the file decodes with the wrong method — or with
    // none — and gives back plausible rubbish rather than an error.
    //
    // Second: only *data* blocks are encoded, and which ones those are is bit 1 of the
    // block id. That bit means the same thing in a 32-bit id as in a 64-bit one, and a
    // reader that lost it while widening would try to decode the XBLOCK and the SLBLOCK
    // and follow the results into nowhere.
    //
    // NDB_CRYPT_CYCLIC is used because it is symmetric — the same call encodes here and
    // decodes there — and because no fixture has ever exercised it against a whole file.
    // Its key is the block's own id, stored in the file next to the data it "protects",
    // which is the entire reason this project never asks for a password.
    let mut bytes = sample(2);
    for (bid, data) in [
        (BID_STORE, STORE),
        (BID_ROOT, ROOT),
        (BID_MSG, MSG),
        (BID_SUBDATA, SUBDATA),
    ] {
        let at = find_block(&bytes, bid).expect("the block should be in the file");
        let mut enc = data.to_vec();
        pstfree::crypt::cyclic(&mut enc, bid as u64);
        bytes[at..at + enc.len()].copy_from_slice(&enc);
        // The checksum is over the bytes as stored, so it moves with them.
        let total = (data.len() + 12).div_ceil(ALIGN) * ALIGN;
        let t = at + total - 12;
        put32(&mut bytes, t + 8, crc32(&enc));
    }
    let partial = crc32(&bytes[8..8 + 471]);
    put32(&mut bytes, 4, partial);

    let p = write("pstfree-ansi-crypt.pst", &bytes);
    let mut pst = Pst::open(p.to_str().unwrap()).unwrap();
    assert_eq!(pst.block(BID_MSG as u64).unwrap(), MSG);
    assert_eq!(pst.block(BID_STORE as u64).unwrap(), STORE);
    // The internal blocks were never encoded and must not be decoded on the way out.
    assert_eq!(pst.node_blocks(BID_XBLOCK as u64).unwrap().len(), 2);
    assert!(pst.warnings.is_empty(), "{:?}", pst.warnings);
}

/// Where a block with this id starts, found the way `carve` finds one: by its trailer.
fn find_block(bytes: &[u8], bid: u32) -> Option<usize> {
    let mut end = ALIGN;
    while end <= bytes.len() {
        let t = end - 12;
        if u32::from_le_bytes(bytes[t + 4..t + 8].try_into().unwrap()) == bid {
            let cb = u16::from_le_bytes(bytes[t..t + 2].try_into().unwrap()) as usize;
            let total = (cb + 12).div_ceil(ALIGN) * ALIGN;
            if total <= end {
                return Some(end - total);
            }
        }
        end += ALIGN;
    }
    None
}
