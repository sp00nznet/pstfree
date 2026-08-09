//! The node database: the header, the two B-trees, and the block index.
//!
//! This is the bottom layer of MS-PST. Everything above it — folders, messages,
//! properties — is reached by looking a node up here and reading its blocks.
//!
//! Nothing in this layer is encoded. Pages and B-tree entries are stored in the clear
//! whether or not the file has a "password", so a full structural survey of a PST needs
//! no key, no Outlook and no permission.

use crate::crypt;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// `!BDN` at offset 0. Every PST and OST starts with it.
const MAGIC: u32 = 0x4E44_4221;
/// `SM` at offset 8 for a PST, `SO` for an OST. Same container either way.
const MAGIC_PST: u16 = 0x4D53;
const MAGIC_OST: u16 = 0x4F53;
const SENTINEL: u8 = 0x80;

// HEADER field offsets, Unicode layout. Named because the numbers are meaningless
// otherwise and the spec is the only place they come from.
const OFF_WVER: usize = 10;
const OFF_ROOT: usize = 180;
const OFF_IB_FILE_EOL: usize = OFF_ROOT + 4;
const OFF_BREF_NBT: usize = OFF_ROOT + 36;
const OFF_BREF_BBT: usize = OFF_ROOT + 52;
const OFF_FAMAP_VALID: usize = OFF_ROOT + 68;
const OFF_SENTINEL: usize = 512;
const OFF_CRYPT_METHOD: usize = 513;
const HEADER_LEN: usize = 564;

const PTYPE_BBT: u8 = 0x80;
const PTYPE_NBT: u8 = 0x81;

/// A corrupt file must not be able to steer us into a loop or a 400-deep recursion.
const MAX_BTREE_DEPTH: u32 = 32;

/// Where the fixed structures sit inside a page. The two Unicode variants differ only
/// here, and only in ways nothing else needs to know about.
///
/// Established by reading real files, because the large-page variant is what Outlook 2013
/// onwards writes for OST and it does not match the 512-byte layout by simple scaling.
/// Three differences, all of them silent — a reader that assumes the 512-byte layout gets
/// plausible-looking rubbish rather than an error:
///
/// - Trailers sit **24** bytes from the end of a page or block, not 16.
/// - Blocks are padded to **512** bytes, not 64.
/// - B-tree entry counts are 16-bit, because 4096 bytes hold more entries than a byte
///   can count.
#[derive(Debug, Clone, Copy)]
struct Layout {
    size: u64,
    /// Start of the 16-byte PAGETRAILER.
    trailer: usize,
    /// Start of the entry-count footer, immediately after the entry array.
    footer: usize,
    /// Whether cEnt/cEntMax are 16-bit.
    wide_counts: bool,
    /// What a block's total length is rounded up to.
    block_align: u64,
    /// How far the BLOCKTRAILER starts from the end of that padded length.
    block_trailer_back: u64,
}

const LAYOUT_512: Layout = Layout {
    size: 512,
    trailer: 496,
    footer: 488,
    wide_counts: false,
    block_align: 64,
    block_trailer_back: 16,
};
const LAYOUT_4K: Layout = Layout {
    size: 4096,
    trailer: 4072,
    footer: 4056,
    wide_counts: true,
    block_align: 512,
    block_trailer_back: 24,
};

/// How data blocks are obfuscated. None of these take a key — the tables are fixed and
/// identical for every PST ever written, which is why a password cannot keep anyone out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crypt {
    None,
    Permute,
    Cyclic,
    Unknown(u8),
}

impl Crypt {
    fn from(b: u8) -> Crypt {
        match b {
            0 => Crypt::None,
            1 => Crypt::Permute,
            2 => Crypt::Cyclic,
            other => Crypt::Unknown(other),
        }
    }
}

/// Block reference: which block, and where it is in the file.
#[derive(Debug, Clone, Copy)]
pub struct Bref {
    pub bid: u64,
    pub ib: u64,
}

/// One entry in the node B-tree. `nid` is the handle the layers above use.
#[derive(Debug, Clone, Copy)]
pub struct Node {
    pub nid: u32,
    pub bid_data: u64,
    pub bid_sub: u64,
    pub nid_parent: u32,
}

/// One entry in the block B-tree: where a block's bytes are and how many there are.
#[derive(Debug, Clone, Copy)]
pub struct Block {
    pub bid: u64,
    pub ib: u64,
    pub cb: u16,
    pub cref: u16,
}

impl Node {
    /// Low 5 bits of the NID. Says what kind of thing the node is.
    pub fn nid_type(&self) -> u8 {
        (self.nid & 0x1F) as u8
    }
}

/// The human-readable name for a NID type, for the survey output.
pub fn nid_type_name(t: u8) -> &'static str {
    match t {
        0x00 => "heap node",
        0x01 => "internal",
        0x02 => "folder",
        0x03 => "search folder",
        0x04 => "message",
        0x05 => "attachment",
        0x06 => "search update queue",
        0x07 => "search criteria",
        0x08 => "associated message",
        0x0A => "contents table index",
        0x0B => "receive folder table",
        0x0C => "outgoing queue table",
        0x0D => "hierarchy table",
        0x0E => "contents table",
        0x0F => "associated contents table",
        0x10 => "search contents table",
        0x11 => "attachment table",
        0x12 => "recipient table",
        0x13 => "search table index",
        0x1F => "property/table context",
        // MS-PST lists 0x09 and 0x14-0x1E as unallocated, but OST files are full of
        // 0x14 and 0x15 - one of each per folder, give or take. Almost certainly the
        // sync engine's per-folder state, which is OST-only and undocumented. Named
        // for what is actually known about them rather than guessed at.
        0x14..=0x1E => "undocumented (OST sync state?)",
        _ => "unallocated in MS-PST",
    }
}

pub struct Pst {
    file: File,
    page: Layout,
    pub crypt: Crypt,
    pub ver: u16,
    /// True when the header says OST rather than PST.
    pub is_ost: bool,
    pub declared_len: u64,
    pub actual_len: u64,
    pub nbt_root: Bref,
    pub bbt_root: Bref,
    /// bid -> where its bytes are. Built on first use; the block B-tree is walked once.
    bbt: Option<HashMap<u64, Block>>,
    /// Everything survivable that looked wrong on the way through. This is the seed of
    /// the damage report — the thing the paid tools replace with a progress bar.
    pub warnings: Vec<String>,
}

pub(crate) fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub(crate) fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64le(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}
fn bref(b: &[u8], o: usize) -> Bref {
    Bref { bid: u64le(b, o), ib: u64le(b, o + 8) }
}

impl Pst {
    pub fn open(path: &str) -> Result<Pst, String> {
        let mut file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
        let actual_len = file.metadata().map_err(|e| e.to_string())?.len();

        // Read what is there rather than requiring a full header, so a file that is a PST
        // but has lost its tail is told apart from a file that was never a PST at all.
        let mut h = Vec::with_capacity(HEADER_LEN);
        file.by_ref()
            .take(HEADER_LEN as u64)
            .read_to_end(&mut h)
            .map_err(|e| format!("{path}: {e}"))?;

        if h.len() < 4 || u32le(&h, 0) != MAGIC {
            return Err(format!(
                "{path}: no !BDN signature. This is not a PST or OST file."
            ));
        }
        if h.len() < HEADER_LEN {
            return Err(format!(
                "{path}: has a PST signature but is only {actual_len} bytes - the {HEADER_LEN}-byte header itself is cut short, so there is nothing left to recover from."
            ));
        }
        let mut warnings = Vec::new();
        let client = u16le(&h, 8);
        if client != MAGIC_PST && client != MAGIC_OST {
            warnings.push(format!(
                "header client signature is 0x{client:04X}, expected 'SM' or 'SO' — file may be damaged"
            ));
        }

        let ver = u16le(&h, OFF_WVER);
        let page = match ver {
            14 | 15 => {
                // ponytail: ANSI has a different header layout and a 2GB ceiling. Refuse
                // it cleanly rather than mis-parse it; add it when a 2002-era archive
                // actually turns up.
                return Err(format!(
                    "{path}: ANSI PST (Outlook 97-2002, format version {ver}). Not supported yet."
                ));
            }
            23 => LAYOUT_512,
            36 | 37 => LAYOUT_4K,
            other => {
                warnings.push(format!(
                    "unrecognised format version {other} — assuming 512-byte pages"
                ));
                LAYOUT_512
            }
        };

        if h[OFF_SENTINEL] != SENTINEL {
            warnings.push(format!(
                "header sentinel is 0x{:02X}, expected 0x80 — header may be damaged",
                h[OFF_SENTINEL]
            ));
        }

        let declared_len = u64le(&h, OFF_IB_FILE_EOL);
        if declared_len > actual_len {
            warnings.push(format!(
                "file is truncated: header says {declared_len} bytes, {actual_len} present ({} missing)",
                declared_len - actual_len
            ));
        }
        if h[OFF_FAMAP_VALID] == 0 {
            warnings.push(
                "allocation map is marked invalid — the file was not closed cleanly".into(),
            );
        }

        Ok(Pst {
            file,
            page,
            crypt: Crypt::from(h[OFF_CRYPT_METHOD]),
            ver,
            is_ost: client == MAGIC_OST,
            declared_len,
            actual_len,
            nbt_root: bref(&h, OFF_BREF_NBT),
            bbt_root: bref(&h, OFF_BREF_BBT),
            bbt: None,
            warnings,
        })
    }

    fn read_page(&mut self, at: Bref) -> Result<Vec<u8>, String> {
        if at.ib == 0 || at.ib + self.page.size > self.actual_len {
            return Err(format!(
                "page offset {} is outside the file (file is {} bytes)",
                at.ib, self.actual_len
            ));
        }
        let mut buf = vec![0u8; self.page.size as usize];
        self.file.seek(SeekFrom::Start(at.ib)).map_err(|e| e.to_string())?;
        self.file.read_exact(&mut buf).map_err(|e| e.to_string())?;

        // PAGETRAILER: ptype, ptypeRepeat, wSig, dwCRC, bid.
        let t = self.page.trailer;
        if buf[t] != buf[t + 1] {
            return Err(format!(
                "page at {} has mismatched type bytes (0x{:02X}/0x{:02X}) — not a page",
                at.ib, buf[t], buf[t + 1]
            ));
        }
        // Low bit of a BID is reserved and is not part of the identity.
        if u64le(&buf, t + 8) & !1 != at.bid & !1 {
            return Err(format!(
                "page at {} claims block id {} but was reached as {} — index points at the wrong place",
                at.ib,
                u64le(&buf, t + 8),
                at.bid
            ));
        }
        // ponytail: the CRC needs the 256-entry table from MS-PST 5.3. Structure and
        // identity are checked above, which catches a torn index; add the CRC when the
        // repair pass needs to tell "wrong page" from "right page, rotten bytes".
        Ok(buf)
    }

    /// Walk a B-tree, calling `leaf` with each leaf entry's bytes.
    ///
    /// Every failure below a node is recorded and the walk continues. A tool that stops
    /// at the first bad page is a tool that cannot read a damaged file, which is the only
    /// kind of file anybody needs help with.
    fn walk<F: FnMut(&[u8])>(
        &mut self,
        at: Bref,
        want: u8,
        depth: u32,
        seen: &mut HashSet<u64>,
        leaf: &mut F,
    ) {
        if depth > MAX_BTREE_DEPTH {
            self.warnings.push(format!("B-tree deeper than {MAX_BTREE_DEPTH} levels at offset {} — stopped", at.ib));
            return;
        }
        if !seen.insert(at.ib) {
            self.warnings
                .push(format!("B-tree loops back to offset {} — stopped", at.ib));
            return;
        }
        let page = match self.read_page(at) {
            Ok(p) => p,
            Err(e) => {
                self.warnings.push(e);
                return;
            }
        };

        let ptype = page[self.page.trailer];
        if ptype != want {
            self.warnings.push(format!(
                "expected a 0x{want:02X} page at offset {} but found 0x{ptype:02X}",
                at.ib
            ));
            return;
        }

        // BTPAGE footer, immediately after the entry array: cEnt, cEntMax, cbEnt, cLevel.
        let f = self.page.footer;
        let (count, size, level) = if self.page.wide_counts {
            (u16le(&page, f) as usize, page[f + 4] as usize, page[f + 5])
        } else {
            (page[f] as usize, page[f + 2] as usize, page[f + 3])
        };
        if size == 0 || count * size > f {
            self.warnings.push(format!(
                "page at {} declares {count} entries of {size} bytes, which does not fit — skipped",
                at.ib
            ));
            return;
        }

        for i in 0..count {
            let e = &page[i * size..i * size + size];
            if level == 0 {
                leaf(e);
            } else {
                // BTENTRY: btkey (8) then the BREF of the child.
                self.walk(bref(e, 8), want, depth + 1, seen, leaf);
            }
        }
    }

    /// Every node in the file, in B-tree order.
    pub fn nodes(&mut self) -> Vec<Node> {
        let mut out = Vec::new();
        let root = self.nbt_root;
        self.walk(root, PTYPE_NBT, 0, &mut HashSet::new(), &mut |e| {
            // NBTENTRY: nid (8, low 4 significant), bidData, bidSub, nidParent, padding.
            out.push(Node {
                nid: u32le(e, 0),
                bid_data: u64le(e, 8),
                bid_sub: u64le(e, 16),
                nid_parent: u32le(e, 24),
            });
        });
        out
    }

    /// A block's bytes, decoded. The low bit of a BID is reserved, so it is masked off
    /// everywhere a block is looked up.
    ///
    /// Only *data* blocks are obfuscated. Internal blocks — the ones holding lists of
    /// other block ids — are stored in the clear, which is how a file with a "password"
    /// can be navigated without one.
    pub fn block(&mut self, bid: u64) -> Result<Vec<u8>, String> {
        if self.bbt.is_none() {
            let map = self.blocks().into_iter().map(|b| (b.bid & !1, b)).collect();
            self.bbt = Some(map);
        }
        let b = *self
            .bbt
            .as_ref()
            .unwrap()
            .get(&(bid & !1))
            .ok_or_else(|| format!("block {bid} is not in the block index"))?;

        // A block is its data, then padding, then the trailer, the whole thing rounded up
        // to the format's block alignment.
        let back = self.page.block_trailer_back;
        let total = (b.cb as u64 + back).div_ceil(self.page.block_align) * self.page.block_align;
        if b.ib + total > self.actual_len {
            return Err(format!(
                "block {bid} runs to offset {} but the file ends at {}",
                b.ib + total,
                self.actual_len
            ));
        }
        let mut buf = vec![0u8; total as usize];
        self.file.seek(SeekFrom::Start(b.ib)).map_err(|e| e.to_string())?;
        self.file.read_exact(&mut buf).map_err(|e| e.to_string())?;

        // BLOCKTRAILER: cb, wSig, dwCRC, bid. The id catches a block index pointing
        // somewhere plausible but wrong.
        let t = (total - back) as usize;
        if u64le(&buf, t + 8) & !1 != bid & !1 {
            return Err(format!(
                "block at offset {} identifies as {} but the index called it {bid}",
                b.ib,
                u64le(&buf, t + 8)
            ));
        }
        // The large-page format's trailer carries eight more bytes than the spec's, and
        // the last two are the inflated length. When it disagrees with the stored length
        // the block is zlib-compressed - which is how a 16MB OST holds far more than
        // 16MB of mail, and is not mentioned anywhere in MS-PST.
        let inflated = if back == 24 { u16le(&buf, t + 18) as usize } else { b.cb as usize };
        buf.truncate(b.cb as usize);

        if bid & 2 == 0 {
            match self.crypt {
                Crypt::None => {}
                Crypt::Permute => crypt::permute_decode(&mut buf),
                Crypt::Cyclic => crypt::cyclic(&mut buf, bid),
                Crypt::Unknown(m) => {
                    return Err(format!("block {bid} uses unknown encoding method 0x{m:02X}"))
                }
            }
        }

        if inflated != b.cb as usize {
            buf = miniz_oxide::inflate::decompress_to_vec_zlib(&buf)
                .map_err(|e| format!("block {bid} is compressed and would not inflate: {e:?}"))?;
            if buf.len() != inflated {
                self.warnings.push(format!(
                    "block {bid} inflated to {} bytes, its trailer said {inflated}",
                    buf.len()
                ));
            }
        }
        Ok(buf)
    }

    /// A node's data as the list of blocks it is made of.
    ///
    /// Anything over 8176 bytes does not fit in one block, so the node points at an
    /// XBLOCK — a list of block ids — or an XXBLOCK, a list of XBLOCKs. Kept as a list
    /// rather than concatenated because heap ids address a specific block by number.
    pub fn node_blocks(&mut self, bid: u64) -> Result<Vec<Vec<u8>>, String> {
        if bid == 0 {
            return Ok(Vec::new());
        }
        let first = self.block(bid)?;
        // Internal blocks are the indirection ones. btype 0x01 is XBLOCK/XXBLOCK.
        if bid & 2 == 0 || first.len() < 8 || first[0] != 0x01 {
            return Ok(vec![first]);
        }
        let level = first[1];
        let count = u16le(&first, 2) as usize;
        if 8 + count * 8 > first.len() {
            return Err(format!("XBLOCK {bid} claims {count} entries, which do not fit"));
        }
        let children: Vec<u64> = (0..count).map(|i| u64le(&first, 8 + i * 8)).collect();

        let mut out = Vec::new();
        for c in children {
            match level {
                // XXBLOCK: each entry is an XBLOCK, so recurse one level.
                2 => out.extend(self.node_blocks(c)?),
                _ => out.push(self.block(c)?),
            }
        }
        Ok(out)
    }

    /// A node's subnode tree, flattened to local id -> the block holding its data.
    ///
    /// Anything too big for a node's own heap — long message bodies, every attachment —
    /// lives out here under its own local id.
    ///
    /// Two block types, distinguished by their level: leaves list the subnodes, and the
    /// level above lists more leaves. Both are internal blocks, so neither is obfuscated.
    ///
    /// ponytail: a leaf entry also carries a subnode tree of its own, which is skipped.
    /// Only attachments nest that far, and nothing reads attachments yet.
    pub fn subnodes(&mut self, bid: u64) -> Result<HashMap<u32, u64>, String> {
        let mut out = HashMap::new();
        if bid == 0 {
            return Ok(out);
        }
        let mut stack = vec![bid];
        let mut seen = HashSet::new();
        while let Some(b) = stack.pop() {
            if !seen.insert(b & !1) {
                self.warnings.push(format!("subnode tree loops back to block {b} — stopped"));
                continue;
            }
            let blk = self.block(b)?;
            if blk.len() < 8 || blk[0] != 0x02 {
                return Err(format!(
                    "block {b} is not a subnode block: type byte is 0x{:02X}, expected 0x02",
                    blk.first().copied().unwrap_or(0)
                ));
            }
            let level = blk[1];
            let count = u16le(&blk, 2) as usize;
            // SLENTRY is nid, bidData, bidSub. SIENTRY is nid and the block below it.
            let width = if level == 0 { 24 } else { 16 };
            if 8 + count * width > blk.len() {
                return Err(format!(
                    "subnode block {b} claims {count} entries of {width} bytes, which do not fit in {}",
                    blk.len()
                ));
            }
            for i in 0..count {
                let e = 8 + i * width;
                if level == 0 {
                    out.insert(u32le(&blk, e), u64le(&blk, e + 8));
                } else {
                    stack.push(u64le(&blk, e + 8));
                }
            }
        }
        Ok(out)
    }

    /// Every block in the file, in B-tree order.
    pub fn blocks(&mut self) -> Vec<Block> {
        let mut out = Vec::new();
        let root = self.bbt_root;
        self.walk(root, PTYPE_BBT, 0, &mut HashSet::new(), &mut |e| {
            // BBTENTRY: BREF (16), cb (2), cRef (2), padding.
            out.push(Block {
                bid: u64le(e, 0),
                ib: u64le(e, 8),
                cb: u16le(e, 16),
                cref: u16le(e, 18),
            });
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real files, fetched by tests/fetch-fixtures.ps1. Skipped rather than failed when
    // absent so a fresh clone still builds and tests green.
    fn open(name: &str) -> Option<Pst> {
        let p = format!("tests/data/{name}");
        if !std::path::Path::new(&p).exists() {
            eprintln!("skipping {name}: run tests/fetch-fixtures.ps1");
            return None;
        }
        Some(Pst::open(&p).expect("fixture should open"))
    }

    fn survey(name: &str) {
        let Some(mut pst) = open(name) else { return };
        assert!(matches!(pst.ver, 23 | 36 | 37), "{name}: format version {}", pst.ver);
        assert!(pst.nbt_root.ib > 0 && pst.bbt_root.ib > 0, "{name} has no B-tree roots");

        let nodes = pst.nodes();
        let blocks = pst.blocks();
        assert!(nodes.len() > 10, "{name}: only {} nodes, walk stopped early", nodes.len());
        assert!(!blocks.is_empty(), "{name}: no blocks found");

        // 0x21 is the message store and 0x122 the root folder. Every intact file has both.
        assert!(nodes.iter().any(|n| n.nid == 0x21), "{name}: no message store node");
        assert!(nodes.iter().any(|n| n.nid == 0x122), "{name}: no root folder node");

        // Every node's data block must actually exist in the block B-tree.
        let known: HashSet<u64> = blocks.iter().map(|b| b.bid & !1).collect();
        let orphans = nodes
            .iter()
            .filter(|n| n.bid_data != 0 && !known.contains(&(n.bid_data & !1)))
            .count();
        assert_eq!(orphans, 0, "{name}: {orphans} nodes point at missing blocks");

        assert!(pst.warnings.is_empty(), "{name}: {:?}", pst.warnings);
    }

    #[test]
    fn reads_a_pst() {
        survey("dist-list.pst");
    }

    #[test]
    fn reads_an_ost() {
        survey("example-2013.ost");
    }

    /// The point of the whole project: a "password-protected" PST is not protected. It
    /// reads identically to any other file and nothing anywhere asks for the password.
    #[test]
    fn password_is_not_a_lock() {
        survey("passworded.pst");
    }

    /// A half-a-file must produce a diagnosis and whatever nodes survive — not a panic,
    /// not a bare "file is corrupt", and not a progress bar. This is the whole pitch, so
    /// it gets a test from day one.
    #[test]
    fn survives_a_truncated_file() {
        let src = "tests/data/dist-list.pst";
        if !std::path::Path::new(src).exists() {
            return;
        }
        let mut bytes = std::fs::read(src).unwrap();
        bytes.truncate(bytes.len() * 6 / 10);
        let cut = std::env::temp_dir().join("pstfree-truncated.pst");
        std::fs::write(&cut, &bytes).unwrap();

        let mut pst = Pst::open(cut.to_str().unwrap()).expect("a truncated PST should still open");
        let nodes = pst.nodes();
        let _ = pst.blocks();

        assert!(
            pst.warnings.iter().any(|w| w.contains("truncated")),
            "no truncation diagnosis, only: {:?}",
            pst.warnings
        );
        // The header and both B-tree roots live near the front, so the survey should
        // still recover the node list even though most of the mail is gone.
        assert!(!nodes.is_empty(), "gave up entirely on a file with an intact header");
        let _ = std::fs::remove_file(cut);
    }

    #[test]
    fn rejects_a_file_that_is_not_a_pst() {
        let e = Pst::open("Cargo.toml").err().expect("Cargo.toml is not a PST");
        assert!(e.contains("not a PST"), "unhelpful error: {e}");
    }
}
