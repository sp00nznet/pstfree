//! Write a damaged PST back out as a clean one.
//!
//! Export gets the mail out; this gets the *file* back. It is what the paid tools sell and
//! the one thing reading alone cannot replace — a store Outlook will open, rather than a
//! folder of `.eml` a person then has to do something with.
//!
//! The trick is that almost nothing has to be understood to do it. A damaged PST is
//! usually a broken *index* over intact *contents*: the blocks holding the heaps, the
//! property contexts and the tables are fine, and only the B-trees, the header and the
//! checksums over them are wrong. So every block is copied byte for byte — encoding,
//! compression and all — and only the index around them is built fresh. Nothing here
//! parses a property or a heap, which is why it cannot corrupt one.
//!
//! One byte of a copied block does change. A block's trailer carries `wSig`, computed from
//! its own file offset and id (MS-PST 5.5), and the offset is exactly what a rebuild
//! moves. Everything else in the trailer — the length, the CRC over the data, the id —
//! describes bytes that did not change, so it is carried across untouched.

use crate::ndb::{crc32, Block, Node, Pst};
use std::io::Write;

/// Where the first Allocation Map lives, and how far apart they are after that.
/// MS-PST 1.3.2.4, and the reason a rebuild cannot simply lay blocks end to end.
const AMAP_FIRST: u64 = 0x4400;
const AMAP_EVERY: u64 = 253_952;
/// A PMap every eighth AMap, immediately after the first one. MS-PST 1.3.2.5.
const PMAP_FIRST: u64 = AMAP_FIRST + 512;
const PMAP_EVERY: u64 = AMAP_EVERY * 8;

const PAGE: u64 = 512;
/// Blocks are allocated on 64-byte boundaries in the 512-byte-page format.
const BLOCK_ALIGN: u64 = 64;
/// BTPAGE: entries, then cEnt, cEntMax, cbEnt, cLevel, padding, and the 16-byte trailer.
const ENTRIES_END: usize = 488;
const TRAILER_AT: usize = 496;
const BBTENTRY: usize = 24;
const NBTENTRY: usize = 32;
const BTENTRY: usize = 24;

const PTYPE_BBT: u8 = 0x80;
const PTYPE_NBT: u8 = 0x81;

// HEADER offsets this code writes. The two id counters sit well before ROOT, which is a
// good enough reason to name them rather than trust a mental picture of the layout.
const OFF_BID_NEXT_P: usize = 32;
const OFF_BID_NEXT_B: usize = 516;

/// The header's own FMap and FPMap cover the first 32MB and the first 2GB. Past 32MB a
/// file needs FMap *pages*, whose positions MS-PST gives only in a diagram — and a
/// rebuild that guessed them would have Outlook write a map over live data the first time
/// it allocated. Refused rather than guessed.
const MAX_REBUILD: u64 = 32 << 20;

pub struct Rebuilt {
    pub nodes: usize,
    pub blocks: usize,
    /// Blocks whose bytes could not be read or did not match their own checksum. Left out,
    /// because a clean file with less in it beats a file that lies about being whole.
    pub dropped_blocks: usize,
    /// Nodes left out because a block they name did not survive.
    pub dropped_nodes: usize,
    /// The nodes without which no reader will open the result, if any went missing.
    ///
    /// Named rather than folded into `dropped_nodes`, because "four nodes were left out"
    /// and "one of them was the message store, so nothing will open this" are completely
    /// different sentences and only one of them is worth interrupting someone for.
    pub missing: Vec<String>,
    pub bytes: u64,
}

/// The message store, and the folder every other folder hangs off. MS-PST 2.4.1 and 2.4.3.
const ESSENTIAL: [(u32, &str); 2] = [(0x21, "message store"), (0x122, "root folder")];

/// MS-PST 5.5: the low 32 bits of the offset XOR the id, folded in half.
fn sig(ib: u64, bid: u64) -> u16 {
    let x = (ib as u32) ^ (bid as u32);
    ((x >> 16) as u16) ^ (x as u16)
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// True where a 512-byte slot is spoken for by a map page and a block must not go.
///
/// The maps themselves are left as zeroes: the header goes out marked INVALID_AMAP, which
/// MS-PST 2.6.1.3.7 defines as "rebuild these before touching the file", so their contents
/// are Outlook's business. Their *space* is this code's business — a rebuild writes them
/// wherever the formula says, and anything living there would be overwritten.
fn reserved(at: u64) -> bool {
    (at >= AMAP_FIRST && (at - AMAP_FIRST).is_multiple_of(AMAP_EVERY))
        || (at >= PMAP_FIRST && (at - PMAP_FIRST).is_multiple_of(PMAP_EVERY))
}

/// The next offset at or after `at`, aligned, whose whole span clears the map slots.
///
/// Blocks align to 64 and pages to 512 — a page landing mid-sector would still *read*,
/// since a BREF is just an offset, but nothing else writes one there and a repair is no
/// place to be the first.
fn place_aligned(mut at: u64, len: u64, align: u64) -> u64 {
    at = at.div_ceil(align) * align;
    loop {
        // A block may not straddle a map page either, so the whole span has to be clear.
        let clash = (at / PAGE..=(at + len - 1) / PAGE)
            .map(|p| p * PAGE)
            .find(|p| reserved(*p));
        match clash {
            Some(p) => at = (p + PAGE).div_ceil(align) * align,
            None => return at,
        }
    }
}

/// One level of a B-tree, bottom up: the pages, and the key each one starts at.
///
/// A page is filled to `per_page` and the next started, which wastes a little space and
/// removes any question of rebalancing. The tree is written once and never inserted into,
/// so there is nothing for balance to buy.
struct Level {
    pages: Vec<(u64, u64, Vec<u8>)>, // (key of first entry, offset, page bytes)
}

fn build_level(
    entries: &[(u64, Vec<u8>)],
    ent_size: usize,
    level: u8,
    ptype: u8,
    next: &mut u64,
    next_bid: &mut u64,
) -> Level {
    let per_page = ENTRIES_END / ent_size;
    let mut pages = Vec::new();

    for chunk in entries.chunks(per_page) {
        let mut page = vec![0u8; PAGE as usize];
        for (i, (_, e)) in chunk.iter().enumerate() {
            page[i * ent_size..i * ent_size + e.len()].copy_from_slice(e);
        }
        page[ENTRIES_END] = chunk.len() as u8;
        page[ENTRIES_END + 1] = per_page as u8;
        page[ENTRIES_END + 2] = ent_size as u8;
        page[ENTRIES_END + 3] = level;

        let at = place_aligned(*next, PAGE, PAGE);
        *next = at + PAGE;
        let bid = *next_bid;
        *next_bid += 4;

        // PAGETRAILER: ptype, the same byte again, wSig, dwCRC over everything before it,
        // then the page's own id.
        page[TRAILER_AT] = ptype;
        page[TRAILER_AT + 1] = ptype;
        put16(&mut page, TRAILER_AT + 2, sig(at, bid));
        put64(&mut page, TRAILER_AT + 8, bid);
        let crc = crc32(&page[..TRAILER_AT]);
        put32(&mut page, TRAILER_AT + 4, crc);

        pages.push((chunk[0].0, at, page));
    }
    Level { pages }
}

/// Build every level of one tree and return its root, as (bid, offset, pages to write).
fn build_tree(
    leaves: Vec<(u64, Vec<u8>)>,
    leaf_size: usize,
    ptype: u8,
    next: &mut u64,
    next_bid: &mut u64,
) -> (u64, u64, Vec<(u64, Vec<u8>)>) {
    let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut level = build_level(&leaves, leaf_size, 0, ptype, next, next_bid);
    let mut depth = 0u8;

    while level.pages.len() > 1 {
        depth += 1;
        // A BTENTRY is the key its child starts at, then that child's BREF.
        let parents: Vec<(u64, Vec<u8>)> = level
            .pages
            .iter()
            .map(|(key, at, page)| {
                let mut e = vec![0u8; BTENTRY];
                put64(&mut e, 0, *key);
                put64(&mut e, 8, u64::from_le_bytes(page[TRAILER_AT + 8..TRAILER_AT + 16].try_into().unwrap()));
                put64(&mut e, 16, *at);
                (*key, e)
            })
            .collect();
        for (_, at, page) in level.pages {
            out.push((at, page));
        }
        level = build_level(&parents, BTENTRY, depth, ptype, next, next_bid);
    }

    let (_, at, page) = level.pages.pop().expect("a tree always has a root page");
    let bid = u64::from_le_bytes(page[TRAILER_AT + 8..TRAILER_AT + 16].try_into().unwrap());
    out.push((at, page));
    (bid, at, out)
}

/// Copy everything readable into a fresh file with a fresh index.
pub fn rebuild(pst: &mut Pst, nodes: &[Node], blocks: &[Block], out: &str) -> Result<Rebuilt, String> {
    if !pst.is_small_page() {
        return Err("only the 512-byte-page Unicode PST can be rebuilt so far. An Outlook \
                    2013 OST uses 4K pages and zlib-compressed blocks, and turning one into \
                    a PST is a format conversion rather than a repair — use --export."
            .into());
    }

    // Blocks first: a node is only worth keeping if the block holding its data survived.
    let mut kept: Vec<(Block, Vec<u8>)> = Vec::new();
    let mut dropped_blocks = 0;
    for b in blocks {
        match pst.raw_block(b) {
            Ok(bytes) if pst.block_intact(b) => kept.push((*b, bytes)),
            _ => dropped_blocks += 1,
        }
    }
    if kept.is_empty() {
        return Err("no block survived well enough to copy — there is nothing to rebuild from".into());
    }
    kept.sort_by_key(|(b, _)| b.bid & !1);

    let mut next = AMAP_FIRST + PAGE;
    let mut placed: Vec<(Block, u64)> = Vec::new();
    let mut body: Vec<(u64, Vec<u8>)> = Vec::new();

    for (b, mut bytes) in kept {
        let at = place_aligned(next, bytes.len() as u64, BLOCK_ALIGN);
        next = at + bytes.len() as u64;

        // The one field a move invalidates. Length, CRC and id all describe bytes that
        // were copied unchanged, so they are carried across as they were.
        let t = bytes.len() - 16;
        put16(&mut bytes, t + 2, sig(at, b.bid));

        placed.push((b, at));
        body.push((at, bytes));
    }

    let known: std::collections::HashSet<u64> = placed.iter().map(|(b, _)| b.bid & !1).collect();
    let mut next_bid = placed.iter().map(|(b, _)| b.bid).max().unwrap_or(4) + 4;

    // BBTENTRY: the block's BREF, then its length and reference count.
    let bbt: Vec<(u64, Vec<u8>)> = placed
        .iter()
        .map(|(b, at)| {
            let mut e = vec![0u8; BBTENTRY];
            put64(&mut e, 0, b.bid);
            put64(&mut e, 8, *at);
            put16(&mut e, 16, b.cb);
            put16(&mut e, 18, if b.cref == 0 { 1 } else { b.cref });
            (b.bid, e)
        })
        .collect();

    let mut dropped_nodes = 0;
    let mut keep: Vec<&Node> = Vec::new();
    for n in nodes {
        let data_ok = n.bid_data == 0 || known.contains(&(n.bid_data & !1));
        let sub_ok = n.bid_sub == 0 || known.contains(&(n.bid_sub & !1));
        if data_ok && sub_ok {
            keep.push(n);
        } else {
            dropped_nodes += 1;
        }
    }
    if keep.is_empty() {
        return Err("no node survived with its data intact — there is nothing to rebuild".into());
    }
    keep.sort_by_key(|n| n.nid);

    let have: std::collections::HashSet<u32> = keep.iter().map(|n| n.nid).collect();
    let missing: Vec<String> = ESSENTIAL
        .iter()
        .filter(|(nid, _)| !have.contains(nid))
        .map(|(nid, what)| format!("{what} (node 0x{nid:X})"))
        .collect();

    let nbt: Vec<(u64, Vec<u8>)> = keep
        .iter()
        .map(|n| {
            let mut e = vec![0u8; NBTENTRY];
            put64(&mut e, 0, n.nid as u64);
            put64(&mut e, 8, n.bid_data);
            put64(&mut e, 16, n.bid_sub);
            put32(&mut e, 24, n.nid_parent);
            (n.nid as u64, e)
        })
        .collect();

    let (nbt_bid, nbt_at, nbt_pages) = build_tree(nbt, NBTENTRY, PTYPE_NBT, &mut next, &mut next_bid);
    let (bbt_bid, bbt_at, bbt_pages) = build_tree(bbt, BBTENTRY, PTYPE_BBT, &mut next, &mut next_bid);

    let eof = next.div_ceil(BLOCK_ALIGN) * BLOCK_ALIGN;
    if eof > MAX_REBUILD {
        return Err(format!(
            "the rebuilt file would be {eof} bytes. Past 32MB a PST needs Free Map pages \
             whose positions MS-PST only draws rather than states, and writing one at a \
             guess would have Outlook overwrite live data. Use --export for a file this big."
        ));
    }

    let mut header = pst.header_bytes()?;
    // Everything the rebuild moved or invalidated. The rest of the header is the original
    // file's and is none of this code's business.
    put64(&mut header, 184, eof); // ibFileEof
    put64(&mut header, 216, nbt_bid); // ROOT.BREFNBT
    put64(&mut header, 224, nbt_at);
    put64(&mut header, 232, bbt_bid); // ROOT.BREFBBT
    put64(&mut header, 240, bbt_at);
    header[248] = 0; // fAMapValid = INVALID_AMAP: rebuild the maps before writing
    let last_amap = AMAP_FIRST + (eof - AMAP_FIRST) / AMAP_EVERY * AMAP_EVERY;
    put64(&mut header, 192, last_amap); // ibAMapLast
    put64(&mut header, 200, 0); // cbAMapFree
    put64(&mut header, 208, 0); // cbPMapFree
    put64(&mut header, OFF_BID_NEXT_P, next_bid);
    put64(&mut header, OFF_BID_NEXT_B, next_bid);
    // dwCRCPartial covers wMagicClient..+471 and dwCRCFull ..+516, so the full one takes
    // in bidNextB and has to be computed after it. Neither range includes either CRC field.
    let partial = crc32(&header[8..8 + 471]);
    put32(&mut header, 4, partial);
    let full = crc32(&header[8..8 + 516]);
    put32(&mut header, 524, full);

    // Lay the whole thing out in memory and write once. Capped at 32MB above, so this is
    // bounded — and a torn write during a repair is the one failure nobody would forgive.
    let mut file = vec![0u8; eof as usize];
    file[..header.len()].copy_from_slice(&header);
    for (at, bytes) in body.iter().chain(nbt_pages.iter()).chain(bbt_pages.iter()) {
        let at = *at as usize;
        file[at..at + bytes.len()].copy_from_slice(bytes);
    }

    let mut f = std::fs::File::create(out).map_err(|e| format!("{out}: {e}"))?;
    f.write_all(&file).map_err(|e| format!("{out}: {e}"))?;

    Ok(Rebuilt {
        nodes: keep.len(),
        blocks: placed.len(),
        dropped_blocks,
        dropped_nodes,
        missing,
        bytes: eof,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rebuilt file must come back clean, and come back the same.
    ///
    /// Clean is the strong half: every checksum this writes is computed over bytes it just
    /// laid down, so a single warning on reopening means a structure was written wrong.
    /// The same is the other half — a repair that loses the mail is not a repair.
    #[test]
    fn a_rebuilt_file_reopens_clean_and_holds_the_same_nodes() {
        let src = "tests/data/dist-list.pst";
        if !std::path::Path::new(src).exists() {
            eprintln!("skipping: run tests/fetch-fixtures.ps1");
            return;
        }
        let mut pst = Pst::open(src).expect("fixture should open");
        let nodes = pst.nodes();
        let blocks = pst.blocks();

        let out = std::env::temp_dir().join("pstfree-rebuild-test.pst");
        let out = out.to_str().unwrap();
        let r = rebuild(&mut pst, &nodes, &blocks, out).expect("rebuild should succeed");
        assert!(r.missing.is_empty(), "rebuilt without {:?}", r.missing);

        let mut back = Pst::open(out).expect("a rebuilt file should open");
        let after = back.nodes();
        let after_blocks = back.blocks();

        // One warning is expected and is the point: the maps go out marked invalid on
        // purpose, and a reader is right to say so. Anything *else* means a structure
        // this code wrote does not check out, since every checksum in the file was
        // computed over bytes it had just laid down.
        let unexpected: Vec<_> = back
            .warnings
            .iter()
            .filter(|w| !w.contains("allocation map"))
            .collect();
        assert!(
            unexpected.is_empty(),
            "a file this wrote should have nothing else wrong with it: {unexpected:?}"
        );
        assert_eq!(after.len(), r.nodes, "node count changed on the way back");
        assert_eq!(after_blocks.len(), r.blocks, "block count changed");

        // Every node has to name the same data, or the index was rebuilt pointing at the
        // wrong thing — which reads perfectly and gives back somebody else's message.
        let before: std::collections::BTreeMap<u32, &Node> =
            nodes.iter().map(|n| (n.nid, n)).collect();
        for n in &after {
            let was = before[&n.nid];
            assert_eq!(
                (was.bid_data, was.bid_sub, was.nid_parent),
                (n.bid_data, n.bid_sub, n.nid_parent),
                "node 0x{:X} came back pointing somewhere else",
                n.nid
            );
        }

        // And the bytes behind them have to survive the move, trailers and all.
        for b in &after_blocks {
            back.block(b.bid)
                .unwrap_or_else(|e| panic!("block {} unreadable after rebuild: {e}", b.bid));
        }
        let _ = std::fs::remove_file(out);
    }

    #[test]
    fn a_map_slot_never_gets_a_block() {
        // The first AMap, the first PMap, and the AMap one interval along.
        for slot in [AMAP_FIRST, PMAP_FIRST, AMAP_FIRST + AMAP_EVERY] {
            assert!(reserved(slot), "0x{slot:X} should be spoken for");
            let at = place_aligned(slot - 64, 512, BLOCK_ALIGN);
            assert!(
                at >= slot + PAGE || at + 512 <= slot,
                "a block at {at} runs through the map page at {slot}"
            );
        }
        assert!(!reserved(AMAP_FIRST + PAGE * 3), "ordinary space is not reserved");
    }
}
