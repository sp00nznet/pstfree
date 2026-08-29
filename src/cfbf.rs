//! Writing Compound File Binary Format — the container a `.msg` file lives in.
//!
//! A compound file is a filesystem inside a file: 512-byte sectors, a FAT chaining them
//! together, and a directory of named streams and storages. Small streams are packed into
//! a "mini stream" at 64-byte granularity with a FAT of its own, because a 40-byte
//! property stream should not cost a whole sector.
//!
//! Only writing is implemented, and only what a `.msg` needs. Everything here is
//! generated, so there is no hostile input to defend against — the risk is the opposite
//! one, of producing a file that looks plausible and that nothing will open. The tests
//! read the result back with an independent implementation.

const SECTOR: usize = 512;
const MINI_SECTOR: usize = 64;
/// Streams smaller than this go in the mini stream. Fixed by the format.
const MINI_CUTOFF: u64 = 4096;

const FREESECT: u32 = 0xFFFF_FFFF;
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
const FATSECT: u32 = 0xFFFF_FFFD;
const DIFSECT: u32 = 0xFFFF_FFFC;
const NOSTREAM: u32 = 0xFFFF_FFFF;

/// Number of FAT sector numbers the header itself can hold before overflow sectors are
/// needed. 109 of them, which covers a file of about 6.8 MB.
const DIFAT_IN_HEADER: usize = 109;

pub enum Item {
    Storage(String, Vec<Item>),
    Stream(String, Vec<u8>),
}

struct Dir {
    name: String,
    kind: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

/// Build a compound file whose root storage holds `children`.
pub fn build(children: Vec<Item>) -> Vec<u8> {
    let mut dirs = vec![Dir {
        name: "Root Entry".into(),
        kind: 5,
        left: NOSTREAM,
        right: NOSTREAM,
        child: NOSTREAM,
        start: FREESECT,
        size: 0,
    }];
    let mut payloads: Vec<(usize, Vec<u8>)> = Vec::new();
    let root_child = add_all(children, &mut dirs, &mut payloads);
    dirs[0].child = root_child;

    // Sectors are handed out in order as things are laid down, and the FAT is written
    // last once the total is known.
    let mut data: Vec<u8> = Vec::new();
    let mut fat: Vec<u32> = Vec::new();
    let mut mini_stream: Vec<u8> = Vec::new();
    let mut minifat: Vec<u32> = Vec::new();

    for (idx, bytes) in payloads {
        if bytes.is_empty() {
            dirs[idx].start = ENDOFCHAIN;
            dirs[idx].size = 0;
        } else if (bytes.len() as u64) < MINI_CUTOFF {
            let first = (mini_stream.len() / MINI_SECTOR) as u32;
            let count = bytes.len().div_ceil(MINI_SECTOR);
            mini_stream.extend_from_slice(&bytes);
            mini_stream.resize(mini_stream.len().next_multiple_of(MINI_SECTOR), 0);
            for i in 0..count {
                minifat.push(if i + 1 == count {
                    ENDOFCHAIN
                } else {
                    first + i as u32 + 1
                });
            }
            dirs[idx].start = first;
            dirs[idx].size = bytes.len() as u64;
        } else {
            dirs[idx].start = chain(&mut data, &mut fat, &bytes);
            dirs[idx].size = bytes.len() as u64;
        }
    }

    // The mini stream is itself an ordinary stream, hanging off the root entry.
    if mini_stream.is_empty() {
        dirs[0].start = ENDOFCHAIN;
    } else {
        dirs[0].size = mini_stream.len() as u64;
        dirs[0].start = chain(&mut data, &mut fat, &mini_stream.clone());
    }

    let minifat_start = if minifat.is_empty() {
        ENDOFCHAIN
    } else {
        let mut b = Vec::new();
        for v in &minifat {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.resize(b.len().next_multiple_of(SECTOR), 0xFF);
        chain(&mut data, &mut fat, &b)
    };
    let minifat_sectors = minifat.len().div_ceil(SECTOR / 4) as u32;

    // Directory entries, four to a sector.
    let mut dirbytes = Vec::new();
    for d in &dirs {
        dirbytes.extend_from_slice(&encode_dir(d));
    }
    dirbytes.resize(dirbytes.len().next_multiple_of(SECTOR), 0);
    // Unused entries in the last sector must read as unallocated, not as a stream.
    for i in dirs.len()..dirbytes.len() / 128 {
        dirbytes[i * 128 + 66] = 0;
        dirbytes[i * 128 + 68..i * 128 + 80].fill(0xFF);
    }
    let dir_start = chain(&mut data, &mut fat, &dirbytes);

    // How many sectors the FAT needs is circular - the FAT has to describe itself, and
    // any overflow sectors describing it. Settled by repeating until it stops growing.
    let (mut n_fat, mut n_difat) = (0usize, 0usize);
    loop {
        let total = fat.len() + n_fat + n_difat;
        let need_fat = total.div_ceil(SECTOR / 4).max(1);
        let need_difat = need_fat
            .saturating_sub(DIFAT_IN_HEADER)
            .div_ceil(SECTOR / 4 - 1);
        if need_fat == n_fat && need_difat == n_difat {
            break;
        }
        (n_fat, n_difat) = (need_fat, need_difat);
    }

    let fat_start = fat.len() as u32;
    fat.resize(fat.len() + n_fat, FATSECT);
    let difat_start = fat.len() as u32;
    fat.resize(fat.len() + n_difat, DIFSECT);
    fat.resize(n_fat * (SECTOR / 4), FREESECT);

    let mut fatbytes = Vec::new();
    for v in &fat {
        fatbytes.extend_from_slice(&v.to_le_bytes());
    }
    data.extend_from_slice(&fatbytes);

    // The DIFAT lists the FAT's own sectors: the first 109 in the header, the rest in
    // overflow sectors each ending with a pointer to the next.
    let fat_ids: Vec<u32> = (0..n_fat as u32).map(|i| fat_start + i).collect();
    let mut difat_bytes = Vec::new();
    let per = SECTOR / 4 - 1;
    for (i, part) in fat_ids[DIFAT_IN_HEADER.min(fat_ids.len())..]
        .chunks(per)
        .enumerate()
    {
        for v in part {
            difat_bytes.extend_from_slice(&v.to_le_bytes());
        }
        for _ in part.len()..per {
            difat_bytes.extend_from_slice(&FREESECT.to_le_bytes());
        }
        let next = if i + 1 < n_difat {
            difat_start + i as u32 + 1
        } else {
            ENDOFCHAIN
        };
        difat_bytes.extend_from_slice(&next.to_le_bytes());
    }
    data.extend_from_slice(&difat_bytes);

    let mut out = header(
        n_fat as u32,
        dir_start,
        minifat_start,
        minifat_sectors,
        if n_difat > 0 { difat_start } else { ENDOFCHAIN },
        n_difat as u32,
        &fat_ids,
    );
    out.extend_from_slice(&data);
    out
}

/// Append bytes as a chain of whole sectors and return the first sector number.
fn chain(data: &mut Vec<u8>, fat: &mut Vec<u32>, bytes: &[u8]) -> u32 {
    let first = fat.len() as u32;
    let count = bytes.len().div_ceil(SECTOR);
    for i in 0..count {
        fat.push(if i + 1 == count {
            ENDOFCHAIN
        } else {
            first + i as u32 + 1
        });
    }
    data.extend_from_slice(bytes);
    data.resize(data.len().next_multiple_of(SECTOR), 0);
    first
}

fn add_all(items: Vec<Item>, dirs: &mut Vec<Dir>, payloads: &mut Vec<(usize, Vec<u8>)>) -> u32 {
    let mut ids = Vec::new();
    for item in items {
        let id = dirs.len() as u32;
        match item {
            Item::Stream(name, bytes) => {
                dirs.push(Dir {
                    name,
                    kind: 2,
                    left: NOSTREAM,
                    right: NOSTREAM,
                    child: NOSTREAM,
                    start: FREESECT,
                    size: 0,
                });
                payloads.push((id as usize, bytes));
            }
            Item::Storage(name, kids) => {
                dirs.push(Dir {
                    name,
                    kind: 1,
                    left: NOSTREAM,
                    right: NOSTREAM,
                    child: NOSTREAM,
                    start: FREESECT,
                    size: 0,
                });
                let c = add_all(kids, dirs, payloads);
                dirs[id as usize].child = c;
            }
        }
        ids.push(id);
    }

    // Siblings form a search tree ordered by name length first and then by uppercased
    // name, which is the format's own comparison and not the obvious one.
    ids.sort_by_key(|&i| {
        let n = &dirs[i as usize].name;
        (n.encode_utf16().count(), n.to_uppercase())
    });
    balanced(&ids, dirs)
}

/// Build a balanced tree from sorted ids, so lookups do not degenerate into a list.
fn balanced(ids: &[u32], dirs: &mut Vec<Dir>) -> u32 {
    if ids.is_empty() {
        return NOSTREAM;
    }
    let mid = ids.len() / 2;
    let left = balanced(&ids[..mid], dirs);
    let right = balanced(&ids[mid + 1..], dirs);
    let me = ids[mid] as usize;
    dirs[me].left = left;
    dirs[me].right = right;
    ids[mid]
}

fn encode_dir(d: &Dir) -> [u8; 128] {
    let mut e = [0u8; 128];
    let utf16: Vec<u16> = d.name.encode_utf16().take(31).collect();
    for (i, c) in utf16.iter().enumerate() {
        e[i * 2..i * 2 + 2].copy_from_slice(&c.to_le_bytes());
    }
    let len = (utf16.len() as u16 + 1) * 2;
    e[64..66].copy_from_slice(&len.to_le_bytes());
    e[66] = d.kind;
    e[67] = 1; // black
    e[68..72].copy_from_slice(&d.left.to_le_bytes());
    e[72..76].copy_from_slice(&d.right.to_le_bytes());
    e[76..80].copy_from_slice(&d.child.to_le_bytes());
    e[116..120].copy_from_slice(&d.start.to_le_bytes());
    e[120..128].copy_from_slice(&d.size.to_le_bytes());
    e
}

#[allow(clippy::too_many_arguments)]
fn header(
    n_fat: u32,
    dir_start: u32,
    minifat_start: u32,
    minifat_count: u32,
    difat_start: u32,
    difat_count: u32,
    fat_ids: &[u32],
) -> Vec<u8> {
    let mut h = vec![0u8; SECTOR];
    h[0..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
    h[24..26].copy_from_slice(&0x003Eu16.to_le_bytes()); // minor version
    h[26..28].copy_from_slice(&0x0003u16.to_le_bytes()); // version 3, 512-byte sectors
    h[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes()); // little endian
    h[30..32].copy_from_slice(&9u16.to_le_bytes()); // 2^9 = 512
    h[32..34].copy_from_slice(&6u16.to_le_bytes()); // 2^6 = 64
    h[44..48].copy_from_slice(&n_fat.to_le_bytes());
    h[48..52].copy_from_slice(&dir_start.to_le_bytes());
    h[56..60].copy_from_slice(&(MINI_CUTOFF as u32).to_le_bytes());
    h[60..64].copy_from_slice(&minifat_start.to_le_bytes());
    h[64..68].copy_from_slice(&minifat_count.to_le_bytes());
    h[68..72].copy_from_slice(&difat_start.to_le_bytes());
    h[72..76].copy_from_slice(&difat_count.to_le_bytes());
    for i in 0..DIFAT_IN_HEADER {
        let v = fat_ids.get(i).copied().unwrap_or(FREESECT);
        h[76 + i * 4..80 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A deliberately independent reader: it follows the header, the FAT and the
    /// directory the way any other implementation would, rather than reusing anything the
    /// writer knows. If the two agree, the writer is producing a real compound file and
    /// not just something its own code understands.
    fn read_back(buf: &[u8]) -> BTreeMap<String, Vec<u8>> {
        assert_eq!(
            &buf[0..8],
            &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1],
            "bad signature"
        );
        let u32at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
        let sector = |n: u32| -> &[u8] {
            let s = SECTOR + n as usize * SECTOR;
            &buf[s..s + SECTOR]
        };

        let n_fat = u32at(44) as usize;
        let mut fat = Vec::new();
        for i in 0..n_fat {
            let id = if i < DIFAT_IN_HEADER {
                u32at(76 + i * 4)
            } else {
                // Follow the overflow chain the same way a reader would.
                let mut d = u32at(68);
                let mut k = i - DIFAT_IN_HEADER;
                while k >= SECTOR / 4 - 1 {
                    d = u32::from_le_bytes(sector(d)[SECTOR - 4..].try_into().unwrap());
                    k -= SECTOR / 4 - 1;
                }
                u32::from_le_bytes(sector(d)[k * 4..k * 4 + 4].try_into().unwrap())
            };
            for c in sector(id).as_chunks::<4>().0 {
                fat.push(u32::from_le_bytes(*c));
            }
        }

        let follow = |start: u32, fat: &Vec<u32>| -> Vec<u8> {
            let mut out = Vec::new();
            let mut s = start;
            let mut guard = 0;
            while s != ENDOFCHAIN && s != FREESECT && (s as usize) < fat.len() {
                out.extend_from_slice(sector(s));
                s = fat[s as usize];
                guard += 1;
                assert!(guard < 100_000, "FAT chain does not terminate");
            }
            out
        };

        let dir = follow(u32at(48), &fat);
        let minifat_raw = follow(u32at(60), &fat);
        let minifat: Vec<u32> = minifat_raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();

        // The root entry's own stream is the mini stream.
        let mini = follow(u32::from_le_bytes(dir[116..120].try_into().unwrap()), &fat);

        let mut out = BTreeMap::new();
        for e in dir.as_chunks::<128>().0 {
            if e[66] != 2 {
                continue; // not a stream
            }
            let nlen = u16::from_le_bytes(e[64..66].try_into().unwrap()) as usize;
            let name: String = String::from_utf16_lossy(
                &e[..nlen.saturating_sub(2)]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect::<Vec<u16>>(),
            );
            let start = u32::from_le_bytes(e[116..120].try_into().unwrap());
            let size = u64::from_le_bytes(e[120..128].try_into().unwrap()) as usize;

            let mut data = if (size as u64) < MINI_CUTOFF {
                let mut d = Vec::new();
                let mut s = start;
                while s != ENDOFCHAIN && (s as usize) < minifat.len() {
                    let o = s as usize * MINI_SECTOR;
                    d.extend_from_slice(&mini[o..o + MINI_SECTOR]);
                    s = minifat[s as usize];
                }
                d
            } else {
                follow(start, &fat)
            };
            data.truncate(size);
            out.insert(name, data);
        }
        out
    }

    #[test]
    fn round_trips_small_streams_through_the_mini_stream() {
        let file = build(vec![
            Item::Stream("__properties_version1.0".into(), vec![7u8; 40]),
            Item::Stream("__substg1.0_0037001F".into(), b"hello".to_vec()),
            Item::Storage(
                "__recip_version1.0_#00000000".into(),
                vec![Item::Stream(
                    "__substg1.0_3001001F".into(),
                    b"someone".to_vec(),
                )],
            ),
        ]);
        assert_eq!(
            file.len() % SECTOR,
            0,
            "file is not a whole number of sectors"
        );

        let got = read_back(&file);
        assert_eq!(
            got.get("__substg1.0_0037001F").map(|v| v.as_slice()),
            Some(&b"hello"[..])
        );
        assert_eq!(
            got.get("__substg1.0_3001001F").map(|v| v.as_slice()),
            Some(&b"someone"[..])
        );
        assert_eq!(got.get("__properties_version1.0"), Some(&vec![7u8; 40]));
    }

    /// Anything over the cutoff takes whole sectors instead, and a big one spans enough
    /// of them to need more than a single FAT sector.
    #[test]
    fn round_trips_a_stream_far_larger_than_the_cutoff() {
        let big: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
        let file = build(vec![
            Item::Stream("__substg1.0_37010102".into(), big.clone()),
            Item::Stream("tiny".into(), b"x".to_vec()),
        ]);
        let got = read_back(&file);
        assert_eq!(
            got.get("__substg1.0_37010102"),
            Some(&big),
            "big stream did not survive"
        );
        assert_eq!(got.get("tiny").map(|v| v.as_slice()), Some(&b"x"[..]));
    }

    /// Enough streams to push the directory past one sector and the FAT past one too.
    #[test]
    fn round_trips_many_streams() {
        let items: Vec<Item> = (0..200)
            .map(|i| Item::Stream(format!("stream{i:04}"), format!("value {i}").into_bytes()))
            .collect();
        let file = build(items);
        let got = read_back(&file);
        assert_eq!(got.len(), 200, "lost streams");
        for i in 0..200 {
            assert_eq!(
                got.get(&format!("stream{i:04}")).map(|v| v.as_slice()),
                Some(format!("value {i}").as_bytes())
            );
        }
    }
}
