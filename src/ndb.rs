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
const OFF_CRC_PARTIAL: usize = 4;
const OFF_CRC_FULL: usize = 524;
/// Both header CRCs are computed from wMagicClient onwards.
const OFF_CRC_RANGE_START: usize = 8;
const OFF_CRC_PARTIAL_END: usize = OFF_CRC_RANGE_START + 471;
const OFF_CRC_FULL_END: usize = OFF_CRC_RANGE_START + 516;

const PTYPE_BBT: u8 = 0x80;
const PTYPE_NBT: u8 = 0x81;

/// A corrupt file must not be able to steer us into a loop or a 400-deep recursion.
const MAX_BTREE_DEPTH: u32 = 32;

/// A thoroughly rotten file can produce a problem per block. Past this many, the list
/// stops being a report and starts being a wall, so the rest are counted instead.
const MAX_WARNINGS: usize = 200;

/// MS-PST's CRC is the ordinary CRC-32 — reflected, polynomial `0xEDB88320` — with no
/// initial value and no final inversion.
///
/// The specification writes it as slicing-by-8 across eight 256-entry tables, which is
/// the identical function computed four bytes at a time. The first of those tables is the
/// standard CRC-32 table, so it is generated here rather than transcribed: ten lines
/// instead of eight kilobytes of constants to get wrong.
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0u32;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

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

/// What sweeping the file for surviving B-tree leaves turned up.
#[derive(Default)]
pub struct Recovered {
    pub nodes: Vec<Node>,
    pub blocks: Vec<Block>,
    pub pages_scanned: u64,
    pub nbt_pages: usize,
    pub bbt_pages: usize,
    /// Pages that look like pages but fail their checksum. These are the losses.
    pub damaged_pages: usize,
    /// Entries found more than once, where the copies had to be chosen between. Normal in
    /// any file that has been written to: freed pages keep their contents.
    pub superseded: usize,
    /// Conflicts where no copy could be confirmed against the bytes on disk, so the
    /// latest was taken on faith. These are the entries most likely to be wrong.
    pub unresolved: usize,
    /// Nodes whose recovered revision cannot be vouched for. Named rather than counted:
    /// "some of your mail may be an old copy" is useless, "this message may be an old
    /// copy" can be checked by the person reading it.
    pub stale: Vec<Stale>,
}

/// A node the sweep could only offer an unconfirmable revision of.
///
/// The honest limit of what can be known here: a PST frees an index page by unlinking it
/// and leaving the bytes, so the sweep finds old entries alongside current ones and has
/// to choose. Where the copies disagree the highest block id wins, because block ids come
/// from a counter that only goes up — but if the page holding the *newest* entry is one of
/// the ones that was lost, the highest id still present is an older revision, and nothing
/// in the file says so. That node reads perfectly and quietly gives you last week's copy.
/// It cannot be fixed. It can be named.
#[derive(Debug, Clone, Copy)]
pub struct Stale {
    pub nid: u32,
    /// Distinct data blocks the surviving entries named. More than one means a choice was
    /// made, and the choice is only as good as the newest entry having survived.
    pub versions: usize,
    /// The chosen entry points at a block that is not in the recovered index at all, so
    /// this revision cannot be read, never mind confirmed as the current one.
    pub dangling: bool,
    /// The data block the chosen entry named, so the finding can be dug into.
    pub bid_data: u64,
}

/// One entry in a node's subnode tree.
#[derive(Debug, Clone, Copy)]
pub struct Sub {
    pub data: u64,
    /// A subnode has a subnode tree of its own. Attachments are where this matters: the
    /// attachment is a subnode of the message, and its bytes hang off the attachment.
    pub sub: u64,
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
    /// Total problems seen, including any past the listing cap.
    suppressed: usize,
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
    Bref {
        bid: u64le(b, o),
        ib: u64le(b, o + 8),
    }
}

impl Pst {
    /// Record a problem, capping the list so a thoroughly rotten file gives a report
    /// rather than a wall of text.
    fn warn(&mut self, msg: String) {
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(msg);
        } else if self.warnings.len() == MAX_WARNINGS {
            self.warnings.push(format!("(more than {MAX_WARNINGS} problems; the rest are counted but not listed)"));
        }
        self.suppressed += 1;
    }

    /// How many problems were found in total, listed or not.
    pub fn problem_count(&self) -> usize {
        self.suppressed.max(self.warnings.len())
    }

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

        // Both header CRCs cover runs starting at wMagicClient. If the partial one fails,
        // the B-tree roots read below cannot be trusted at all, which is exactly the case
        // where scanning the file beats following them.
        for (name, stored, range) in [
            ("partial", u32le(&h, OFF_CRC_PARTIAL), OFF_CRC_RANGE_START..OFF_CRC_PARTIAL_END),
            ("full", u32le(&h, OFF_CRC_FULL), OFF_CRC_RANGE_START..OFF_CRC_FULL_END),
        ] {
            let found = crc32(&h[range]);
            if found != stored {
                warnings.push(format!(
                    "header {name} checksum is 0x{stored:08X} but the bytes give 0x{found:08X} — the header has been altered or damaged"
                ));
            }
        }

        let declared_len = u64le(&h, OFF_IB_FILE_EOL);
        if declared_len > actual_len {
            warnings.push(format!(
                "file is truncated: header says {declared_len} bytes, {actual_len} present ({} missing)",
                declared_len - actual_len
            ));
        }
        if h[OFF_FAMAP_VALID] == 0 {
            warnings
                .push("allocation map is marked invalid — the file was not closed cleanly".into());
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
            suppressed: warnings.len(),
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
        self.file
            .seek(SeekFrom::Start(at.ib))
            .map_err(|e| e.to_string())?;
        self.file.read_exact(&mut buf).map_err(|e| e.to_string())?;

        // PAGETRAILER: ptype, ptypeRepeat, wSig, dwCRC, bid.
        let t = self.page.trailer;
        if buf[t] != buf[t + 1] {
            return Err(format!(
                "page at {} has mismatched type bytes (0x{:02X}/0x{:02X}) — not a page",
                at.ib,
                buf[t],
                buf[t + 1]
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
        // The identity check above catches an index pointing at the wrong page. The CRC
        // catches the other failure: the right page, with rotten bytes inside it. Told
        // apart because the repair needs to know which - one is a bad pointer, the other
        // is lost data.
        let stored = u32le(&buf, t + 4);
        let found = crc32(&buf[..t]);
        if stored != found {
            self.warn(format!(
                "page at {} fails its checksum (stored 0x{stored:08X}, computed 0x{found:08X}) — its contents are damaged",
                at.ib
            ));
        }
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
            self.warnings.push(format!(
                "B-tree deeper than {MAX_BTREE_DEPTH} levels at offset {} — stopped",
                at.ib
            ));
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
        // cbEnt is a number off the page, so it cannot be trusted to be big enough for the
        // entry it claims to describe. Every reader below indexes at a fixed offset — a
        // BTENTRY's BREF ends at 24, an NBTENTRY's nidParent at 28, a BBTENTRY's cRef at
        // 20 — so a short slice runs off the end of itself. On a damaged file that is a
        // panic instead of a diagnosis, which is the one outcome this tool cannot have.
        let need = if level > 0 {
            24
        } else if want == PTYPE_NBT {
            28
        } else {
            20
        };
        if size < need {
            self.warnings.push(format!(
                "page at {} declares {size}-byte entries, too small for the {need} bytes one holds — skipped",
                at.ib
            ));
            return;
        }
        if count * size > f {
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
        self.file
            .seek(SeekFrom::Start(b.ib))
            .map_err(|e| e.to_string())?;
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
        let inflated = if back == 24 {
            u16le(&buf, t + 18) as usize
        } else {
            b.cb as usize
        };
        let stored_crc = u32le(&buf, t + 4);
        buf.truncate(b.cb as usize);

        // Checked over the bytes as stored, before any decoding, because that is what the
        // checksum was computed over. A block that fails here is the case no free tool
        // reports: the index is fine and the data underneath it has rotted.
        let found_crc = crc32(&buf);
        if stored_crc != found_crc {
            self.warn(format!(
                "block {bid} at offset {} fails its checksum (stored 0x{stored_crc:08X}, computed 0x{found_crc:08X}) — {} bytes of damaged data",
                b.ib, b.cb
            ));
        }

        if bid & 2 == 0 {
            match self.crypt {
                Crypt::None => {}
                Crypt::Permute => crypt::permute_decode(&mut buf),
                Crypt::Cyclic => crypt::cyclic(&mut buf, bid),
                Crypt::Unknown(m) => {
                    return Err(format!(
                        "block {bid} uses unknown encoding method 0x{m:02X}"
                    ))
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
            return Err(format!(
                "XBLOCK {bid} claims {count} entries, which do not fit"
            ));
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
    pub fn subnodes(&mut self, bid: u64) -> Result<HashMap<u32, Sub>, String> {
        let mut out = HashMap::new();
        if bid == 0 {
            return Ok(out);
        }
        let mut stack = vec![bid];
        let mut seen = HashSet::new();
        while let Some(b) = stack.pop() {
            if !seen.insert(b & !1) {
                self.warnings
                    .push(format!("subnode tree loops back to block {b} — stopped"));
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
                    out.insert(
                        u32le(&blk, e),
                        Sub {
                            data: u64le(&blk, e + 8),
                            sub: u64le(&blk, e + 16),
                        },
                    );
                } else {
                    stack.push(u64le(&blk, e + 8));
                }
            }
        }
        Ok(out)
    }

    /// Rebuild both indexes by sweeping the file for surviving B-tree leaf pages,
    /// ignoring the header's roots and every branch page entirely.
    ///
    /// This is the part `scanpst.exe` does not do. A torn index is the ordinary way a PST
    /// dies — one bad page high in the tree orphans everything beneath it — but the leaves
    /// holding the actual node and block entries are spread across the whole file and are
    /// almost always still there. Pages sit at fixed offsets and carry a checksum, so they
    /// can be found without reference to anything that points at them, and a page that
    /// checksums correctly is a page, not a coincidence.
    pub fn scan(&mut self) -> Recovered {
        let mut r = Recovered::default();
        // Every copy of every entry is kept, with the offset it was found at, and the
        // conflicts are settled afterwards by checking which copy the bytes agree with.
        let mut nodes: HashMap<u32, Vec<(u64, Node)>> = HashMap::new();
        let mut blocks: HashMap<u64, Vec<(u64, Block)>> = HashMap::new();

        let step = self.page.size;
        let (t, f) = (self.page.trailer, self.page.footer);
        let mut buf = vec![0u8; step as usize];
        let mut ib = step; // offset 0 is the header, never a page

        while ib + step <= self.actual_len {
            r.pages_scanned += 1;
            if self.file.seek(SeekFrom::Start(ib)).is_err()
                || self.file.read_exact(&mut buf).is_err()
            {
                break;
            }
            ib += step;

            let ptype = buf[t];
            if ptype != buf[t + 1] || (ptype != PTYPE_NBT && ptype != PTYPE_BBT) {
                continue;
            }
            // The checksum is what makes this safe: without it, any 512 bytes that
            // happened to hold two equal bytes in the right place would be "a page".
            if crc32(&buf[..t]) != u32le(&buf, t + 4) {
                r.damaged_pages += 1;
                continue;
            }

            let (count, size, level) = if self.page.wide_counts {
                (u16le(&buf, f) as usize, buf[f + 4] as usize, buf[f + 5])
            } else {
                (buf[f] as usize, buf[f + 2] as usize, buf[f + 3])
            };
            if level != 0 || size == 0 || count * size > f {
                continue;
            }
            let at = ib - step;

            if ptype == PTYPE_NBT {
                r.nbt_pages += 1;
                for i in 0..count {
                    let e = &buf[i * size..i * size + size];
                    let n = Node {
                        nid: u32le(e, 0),
                        bid_data: u64le(e, 8),
                        bid_sub: u64le(e, 16),
                        nid_parent: u32le(e, 24),
                    };
                    nodes.entry(n.nid).or_default().push((at, n));
                }
            } else {
                r.bbt_pages += 1;
                for i in 0..count {
                    let e = &buf[i * size..i * size + size];
                    let b = Block {
                        bid: u64le(e, 0),
                        ib: u64le(e, 8),
                        cb: u16le(e, 16),
                        cref: u16le(e, 18),
                    };
                    blocks.entry(b.bid & !1).or_default().push((at, b));
                }
            }
        }

        // Settle the block conflicts by reading the bytes. A freed entry generally points
        // at space that has since been reused, so it fails its own identity or checksum
        // and the live one does not. Where nothing validates, the latest copy is taken -
        // it is the best guess left, and better than nothing at all.
        let mut chosen: Vec<(u64, Vec<(u64, Block)>)> = blocks.into_iter().collect();
        for (_, v) in chosen.iter_mut() {
            v.sort_by_key(|(off, _)| *off);
        }
        for (_, v) in &chosen {
            r.superseded += v.len() - 1;
        }
        let mut live: HashMap<u64, Block> = HashMap::new();
        for (bid, v) in chosen {
            let pick = if v.len() == 1 {
                v[0].1
            } else {
                let mut found = None;
                for (_, b) in v.iter().rev() {
                    if self.block_intact(b) {
                        found = Some(*b);
                        break;
                    }
                }
                match found {
                    Some(b) => b,
                    None => {
                        r.unresolved += 1;
                        v.last().unwrap().1
                    }
                }
            };
            live.insert(bid, pick);
        }

        // Node entries carry no checksum of their own, so they are settled two ways. A
        // node whose data block did not survive is no use whatever it says, so those lose
        // first. Among the rest the highest block id wins: a PST hands out block ids from
        // a counter that only ever goes up, so the largest is the most recently written.
        // That is a stronger signal than file position, which only says where the
        // allocator happened to find room.
        for (nid, mut v) in nodes {
            r.superseded += v.len() - 1;
            // bid_sub breaks the tie when two copies name the same data block, and it has
            // to: a node whose subnode tree moved but whose data did not is a real and
            // ordinary edit — a message gaining an attachment does exactly that. Ranking
            // by data block alone leaves those settled by file position, which says only
            // where the allocator found room, and on the test PST it picks the older
            // subnode tree for one node out of 129. Block ids come off a counter that only
            // ever goes up, so the same argument that makes bid_data a good signal makes
            // bid_sub one too.
            v.sort_by_key(|(_, n)| {
                (
                    live.contains_key(&(n.bid_data & !1)),
                    n.bid_data,
                    n.bid_sub,
                )
            });
            let pick = v.last().unwrap().1;

            // Duplicate copies of the *same* entry are the normal case and say nothing —
            // a page was rewritten and the old one still lies there naming the same block.
            // Only copies that disagree about which block holds the node's data mean a
            // revision was chosen between, so that is what gets reported.
            let versions = v
                .iter()
                .map(|(_, n)| (n.bid_data & !1, n.bid_sub & !1))
                .collect::<HashSet<(u64, u64)>>()
                .len();
            let dangling = pick.bid_data != 0 && !live.contains_key(&(pick.bid_data & !1));
            if versions > 1 || dangling {
                r.stale.push(Stale {
                    nid,
                    versions,
                    dangling,
                    bid_data: pick.bid_data,
                });
            }
            r.nodes.push(Node { nid, ..pick });
        }

        r.stale.sort_by_key(|s| s.nid);
        r.nodes.sort_by_key(|n| n.nid);
        r.blocks = live.into_values().collect();
        r.blocks.sort_by_key(|b| b.bid);
        r
    }

    /// Rebuild the block index from the blocks themselves, using no index at all.
    ///
    /// The last resort, and the strongest one. Every block ends with a trailer holding its
    /// own length, id and checksum, and blocks are padded to a fixed alignment — so the
    /// trailer of any block sits a fixed distance back from an aligned boundary. Testing
    /// every boundary finds every block whose bytes are still intact, whether or not
    /// anything in the file still points at it.
    ///
    /// This is what recovers data after the index pages holding it are gone. It cannot be
    /// fooled into inventing a block: a candidate is only accepted when the checksum in
    /// the trailer matches the bytes in front of it.
    pub fn carve(&mut self) -> Vec<Block> {
        let back = self.page.block_trailer_back;
        let align = self.page.block_align;
        // Later copies win: a block written after another at the same id is the newer of
        // the two, and the file grows forwards.
        let mut found: HashMap<u64, Block> = HashMap::new();

        let mut buf = Vec::new();
        let mut trailer = [0u8; 24];
        let trailer_len = (back as usize).min(trailer.len());

        // Every candidate is a block *end*: an aligned boundary with a trailer just
        // before it. The trailer's own length field then says where the block began.
        let mut end = align;
        while end <= self.actual_len {
            let t = end - back;
            let ok = self.file.seek(SeekFrom::Start(t)).is_ok()
                && self.file.read_exact(&mut trailer[..trailer_len]).is_ok();
            if !ok {
                break;
            }
            let cb = u16le(&trailer, 0) as u64;
            let bid = u64le(&trailer, 8);
            let total = (cb + back).div_ceil(align) * align;

            // A block of this length, ending here, must start at or after the file start,
            // and must be padded to exactly this boundary - otherwise the trailer belongs
            // to something else and these bytes only look like one.
            if cb > 0 && bid > 0 && total <= end {
                let begin = end - total;
                buf.resize(cb as usize, 0);
                if self.file.seek(SeekFrom::Start(begin)).is_ok()
                    && self.file.read_exact(&mut buf).is_ok()
                    && crc32(&buf) == u32le(&trailer, 4)
                {
                    found.insert(bid & !1, Block { bid, ib: begin, cb: cb as u16, cref: 0 });
                }
            }
            end += align;
        }

        let mut out: Vec<Block> = found.into_values().collect();
        out.sort_by_key(|b| b.bid);
        out
    }

    /// Whether a block index entry actually describes a live block: the bytes at that
    /// offset must identify as that block and match their own checksum.
    ///
    /// This is what settles a conflict between two swept entries for the same id. A freed
    /// entry usually points at space that has since been reused, so it fails one or both.
    pub fn block_intact(&mut self, b: &Block) -> bool {
        let back = self.page.block_trailer_back;
        let total = (b.cb as u64 + back).div_ceil(self.page.block_align) * self.page.block_align;
        if b.ib == 0 || b.ib + total > self.actual_len {
            return false;
        }
        let mut buf = vec![0u8; total as usize];
        if self.file.seek(SeekFrom::Start(b.ib)).is_err() || self.file.read_exact(&mut buf).is_err()
        {
            return false;
        }
        let t = (total - back) as usize;
        u64le(&buf, t + 8) & !1 == b.bid & !1
            && crc32(&buf[..b.cb as usize]) == u32le(&buf, t + 4)
    }

    /// Use a recovered block index instead of the one reached from the header.
    pub fn adopt(&mut self, blocks: &[Block]) {
        self.bbt = Some(blocks.iter().map(|b| (b.bid & !1, *b)).collect());
    }

    /// A block exactly as it sits on disk: data, padding and trailer, nothing decoded.
    ///
    /// [`block`](Self::block) hands back the *contents* — deobfuscated and, on an OST,
    /// inflated. Copying a block into a rebuilt file wants the opposite: the bytes
    /// untouched, so whatever encoding they carry keeps working without this ever having
    /// to understand it.
    pub fn raw_block(&mut self, b: &Block) -> Result<Vec<u8>, String> {
        let back = self.page.block_trailer_back;
        let total = (b.cb as u64 + back).div_ceil(self.page.block_align) * self.page.block_align;
        if b.ib + total > self.actual_len {
            return Err(format!("block {} runs past the end of the file", b.bid));
        }
        let mut buf = vec![0u8; total as usize];
        self.file
            .seek(SeekFrom::Start(b.ib))
            .map_err(|e| e.to_string())?;
        self.file.read_exact(&mut buf).map_err(|e| e.to_string())?;
        Ok(buf)
    }

    /// The file's own header bytes, for a rebuild to start from rather than invent.
    ///
    /// Most of a header is fields a repair has no opinion about — the crypt method, the
    /// client version, the reserved runs. Copying it and overwriting only what moved is
    /// both less code and less to get wrong than building all 564 bytes from the spec.
    pub fn header_bytes(&mut self) -> Result<Vec<u8>, String> {
        let mut h = vec![0u8; HEADER_LEN];
        self.file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        self.file.read_exact(&mut h).map_err(|e| e.to_string())?;
        Ok(h)
    }

    /// 512-byte pages, the layout MS-PST actually documents. The 4K variant an Outlook
    /// 2013 OST uses is a different beast and cannot be written by the same code.
    pub fn is_small_page(&self) -> bool {
        self.page.size == 512
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
    use std::collections::BTreeMap;

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
        assert!(
            matches!(pst.ver, 23 | 36 | 37),
            "{name}: format version {}",
            pst.ver
        );
        assert!(
            pst.nbt_root.ib > 0 && pst.bbt_root.ib > 0,
            "{name} has no B-tree roots"
        );

        let nodes = pst.nodes();
        let blocks = pst.blocks();
        assert!(
            nodes.len() > 10,
            "{name}: only {} nodes, walk stopped early",
            nodes.len()
        );
        assert!(!blocks.is_empty(), "{name}: no blocks found");

        // 0x21 is the message store and 0x122 the root folder. Every intact file has both.
        assert!(
            nodes.iter().any(|n| n.nid == 0x21),
            "{name}: no message store node"
        );
        assert!(
            nodes.iter().any(|n| n.nid == 0x122),
            "{name}: no root folder node"
        );

        // Every node's data block must actually exist in the block B-tree.
        let known: HashSet<u64> = blocks.iter().map(|b| b.bid & !1).collect();
        let orphans = nodes
            .iter()
            .filter(|n| n.bid_data != 0 && !known.contains(&(n.bid_data & !1)))
            .count();
        assert_eq!(
            orphans, 0,
            "{name}: {orphans} nodes point at missing blocks"
        );

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

    /// The sweep must reach the same answer as the file's own index, on every node the
    /// two have in common.
    ///
    /// This is the only ground truth available for recovery. An undamaged file carries the
    /// authoritative index *and* the freed pages the sweep reads, so the sweep can be run
    /// against a file whose right answer is already known — and where the two disagree,
    /// the sweep is simply wrong. It caught a real one: with the tie broken on the data
    /// block alone, one node in dist-list.pst came back with an older subnode tree.
    ///
    /// The sweep finding *extra* nodes is not a disagreement. Those are deleted items whose
    /// entries the index dropped and the freed pages kept, and digging them out is the
    /// entire point of sweeping.
    #[test]
    fn the_sweep_agrees_with_the_index_it_replaces() {
        for name in ["dist-list.pst", "example-2013.ost", "passworded.pst"] {
            let Some(mut pst) = open(name) else { return };
            let live: BTreeMap<u32, Node> = pst.nodes().into_iter().map(|n| (n.nid, n)).collect();
            assert!(!live.is_empty(), "{name}: no live index to compare against");

            let swept = pst.scan().nodes;
            let mut shared = 0;
            for n in &swept {
                let Some(l) = live.get(&n.nid) else { continue };
                shared += 1;
                assert_eq!(
                    (l.bid_data, l.bid_sub, l.nid_parent),
                    (n.bid_data, n.bid_sub, n.nid_parent),
                    "{name}: sweep recovered node 0x{:X} at the wrong revision",
                    n.nid
                );
            }
            assert!(
                shared * 10 >= live.len() * 9,
                "{name}: sweep only found {shared} of the {} indexed nodes",
                live.len()
            );
        }
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
        assert!(
            !nodes.is_empty(),
            "gave up entirely on a file with an intact header"
        );
        let _ = std::fs::remove_file(cut);
    }

    /// Write a copy of a fixture with the named B-tree root pages zeroed out.
    fn tear(name: &str, kill_nbt: bool, kill_bbt: bool) -> Option<String> {
        let src = format!("tests/data/{name}");
        if !std::path::Path::new(&src).exists() {
            return None;
        }
        let mut b = std::fs::read(&src).unwrap();
        // The two BREFs in the header ROOT structure point at the B-tree root pages.
        let nbt = u64le(&b, OFF_BREF_NBT + 8) as usize;
        let bbt = u64le(&b, OFF_BREF_BBT + 8) as usize;
        for (kill, at) in [(kill_nbt, nbt), (kill_bbt, bbt)] {
            if kill {
                b[at..at + 512].fill(0);
            }
        }
        let out = std::env::temp_dir().join(format!("pstfree-torn-{kill_nbt}{kill_bbt}-{name}"));
        std::fs::write(&out, &b).unwrap();
        Some(out.to_str().unwrap().to_string())
    }

    /// Every checksum in an undamaged file must verify. This is what stands behind the
    /// CRC being the right algorithm over the right bytes — hundreds of real pages and
    /// blocks written by Outlook, not a test vector.
    #[test]
    fn every_checksum_in_an_intact_file_verifies() {
        for name in ["dist-list.pst", "passworded.pst", "example-2013.ost"] {
            let Some(mut pst) = open(name) else { return };
            let blocks = pst.blocks();
            let _ = pst.nodes();
            for b in &blocks {
                let _ = pst.block(b.bid);
            }
            assert!(blocks.len() > 100, "{name}: only {} blocks checked", blocks.len());
            assert!(pst.warnings.is_empty(), "{name}: {:?}", pst.warnings);
        }
    }

    /// Carving must find the live blocks without consulting any index at all.
    #[test]
    fn carving_finds_the_blocks_the_index_lists() {
        let Some(mut pst) = open("dist-list.pst") else { return };
        let listed: HashSet<u64> = pst.blocks().iter().map(|b| b.bid & !1).collect();
        let carved: HashSet<u64> = pst.carve().iter().map(|b| b.bid & !1).collect();
        let missing: Vec<_> = listed.difference(&carved).collect();
        assert!(missing.is_empty(), "carving missed {} live blocks: {missing:?}", missing.len());
    }

    /// The whole point of the project, as a test. A file whose node B-tree root is gone
    /// still gives up every node it had, and each one still points at the same data.
    #[test]
    fn recovers_everything_from_a_destroyed_node_index() {
        let Some(mut good) = open("dist-list.pst") else { return };
        let want: BTreeMap<u32, u64> =
            good.nodes().iter().map(|n| (n.nid, n.bid_data)).collect();

        let torn = tear("dist-list.pst", true, false).unwrap();
        let mut pst = Pst::open(&torn).unwrap();
        assert!(pst.nodes().is_empty(), "the node index survived being zeroed");

        let got: BTreeMap<u32, u64> =
            pst.scan().nodes.iter().map(|n| (n.nid, n.bid_data)).collect();
        assert_eq!(got, want, "sweeping did not recover the original node table");
    }

    /// And with *both* indexes destroyed, carving the blocks out of the file rebuilds
    /// enough to get back to the same answer.
    #[test]
    fn recovers_everything_from_both_indexes_destroyed() {
        let Some(mut good) = open("dist-list.pst") else { return };
        let want: BTreeMap<u32, u64> =
            good.nodes().iter().map(|n| (n.nid, n.bid_data)).collect();
        let live: BTreeMap<u64, u64> =
            good.blocks().iter().map(|b| (b.bid & !1, b.ib)).collect();

        let torn = tear("dist-list.pst", true, true).unwrap();
        let mut pst = Pst::open(&torn).unwrap();
        assert!(pst.nodes().is_empty() && pst.blocks().is_empty(), "an index survived");

        let carved = pst.carve();
        for b in &carved {
            if let Some(&ib) = live.get(&(b.bid & !1)) {
                assert_eq!(b.ib, ib, "carved block {} at the wrong offset", b.bid);
            }
        }
        let got: BTreeMap<u32, u64> =
            pst.scan().nodes.iter().map(|n| (n.nid, n.bid_data)).collect();
        assert_eq!(got, want, "recovery did not reproduce the original node table");
    }

    #[test]
    fn rejects_a_file_that_is_not_a_pst() {
        let e = Pst::open("Cargo.toml")
            .err()
            .expect("Cargo.toml is not a PST");
        assert!(e.contains("not a PST"), "unhelpful error: {e}");
    }
}
