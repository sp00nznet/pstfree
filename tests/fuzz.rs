//! Random damage, fed straight back in.
//!
//! The curated damage tests break a file in one deliberate way — zeroed roots, a cut
//! tail — and check that the right thing is recovered. This does the opposite: it splats
//! random bytes over random spots and checks only that nothing panics and nothing hangs.
//! Real failing disks do not aim, and the parsers that read a damaged file are exactly
//! the ones handed lengths and counts nobody validated.
//!
//! Seeds are fixed, so a failure is reproducible and the file that caused it is left on
//! disk to look at. This is still self-inflicted damage and no substitute for a real
//! ruined PST — it proves robustness, not that recovery gets the right answer.

use pstfree::ltp::read_node_pc;
use pstfree::ndb::Pst;
use std::sync::mpsc;
use std::time::Duration;

/// Damage is aligned and sized in these, because that is the unit a disk fails in.
const SECTOR: usize = 512;

/// xorshift64*, four lines and no dependency, seeded per round so any failure replays.
fn next(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Every read path a damaged file reaches: the indexes, both recovery routes, and the
/// property parser on each node. Errors are the expected outcome and are ignored — only
/// a panic or a hang is a failure.
fn read_everything(path: &str) {
    let Ok(mut pst) = Pst::open(path) else { return };
    let nodes = pst.nodes();
    let _ = pst.blocks();
    let _ = pst.scan();
    let _ = pst.carve();
    for n in &nodes {
        let _ = read_node_pc(&mut pst, n);
    }
}

#[test]
fn random_damage_never_panics_or_hangs() {
    // 8 rounds a file keeps the suite quick and still lands ~40 damaged regions. The
    // seeds are stable, so raising this only ever adds cases — it never renumbers the
    // ones already passing, and a round that failed at 500 still fails at 500 tomorrow.
    let rounds: u64 = std::env::var("PSTFREE_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);

    const FIXTURES: [&str; 2] = ["dist-list.pst", "example-2013.ost"];

    for (i, name) in FIXTURES.iter().enumerate() {
        let src = format!("tests/data/{name}");
        if !std::path::Path::new(&src).exists() {
            eprintln!("skipping {name}: run tests/fetch-fixtures.ps1");
            continue;
        }
        let clean = std::fs::read(&src).unwrap();
        // The other fixture, as a donor for foreign-data damage. It stands in for whatever
        // else the filesystem might have written over this file. Falls back to this file's
        // own bytes when the other fixture was not fetched.
        let other = std::fs::read(format!("tests/data/{}", FIXTURES[1 - i])).unwrap_or_else(|_| clean.clone());

        for round in 1..=rounds {
            let mut rng = round.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut bytes = clean.clone();

            for _ in 0..1 + next(&mut rng) % 8 {
                // Sector-aligned, because a disk does not fail at byte granularity: a bad
                // sector loses all 512 bytes of itself and none of its neighbour's.
                //
                // Half the runs land in the first 16KB. Uniform offsets almost never hit
                // it, and that is where every field a parser trusts lives — the header,
                // the B-tree roots, the first index pages. Damage to a mail body is a
                // wrong subject line; damage here is the count someone loops on.
                let reach = if next(&mut rng) & 1 == 0 {
                    bytes.len().min(16 * 1024)
                } else {
                    bytes.len()
                };
                let at = (next(&mut rng) as usize % reach) / SECTOR * SECTOR;
                let end = (at + (1 + next(&mut rng) as usize % 8) * SECTOR).min(bytes.len());

                match next(&mut rng) % 5 {
                    // A sector that reads back as zeroes: plausible-but-empty structure,
                    // counts of nothing, offsets to the front of the file.
                    0 => bytes[at..end].fill(0),
                    // A sector that reads back as noise: absurd lengths and ids, which is
                    // what makes a parser allocate or loop rather than quietly do nothing.
                    1 => bytes[at..end].iter_mut().for_each(|b| *b = next(&mut rng) as u8),
                    // Bit rot. One flipped bit leaves every structure intact and only the
                    // checksums disagreeing, which is the one case the whole verify path
                    // exists for and the one a wipe never produces.
                    2 => {
                        let i = at + next(&mut rng) as usize % (end - at);
                        bytes[i] ^= 1 << (next(&mut rng) % 8);
                    }
                    // A torn write, and the reason the image idea was tempting: sectors
                    // from elsewhere in this same file land here. The bytes are a real
                    // page with a real checksum, just the wrong one — so nothing looks
                    // damaged, it looks like a different node than it is. This is the
                    // shape of the stale-revision and ghost-block problem.
                    3 => {
                        let from = (next(&mut rng) as usize % bytes.len()) / SECTOR * SECTOR;
                        let n = (end - at).min(bytes.len() - from);
                        bytes.copy_within(from..from + n, at);
                    }
                    // Foreign data: the filesystem gave this space to another file. The
                    // other fixture is the honest source, since carving must not mistake
                    // an OST's blocks for this file's own.
                    _ => {
                        let n = (end - at).min(other.len());
                        let from = next(&mut rng) as usize % (other.len() - n + 1);
                        bytes[at..at + n].copy_from_slice(&other[from..from + n]);
                    }
                }
            }

            let broken = std::env::temp_dir().join(format!("pstfree-fuzz-{round}-{name}"));
            std::fs::write(&broken, &bytes).unwrap();
            let path = broken.to_str().unwrap().to_string();

            // A hung parser cannot be killed from here, so the thread is left running and
            // the assert carries the diagnosis. Panic output is deliberately not
            // suppressed: a passing run prints nothing, and a failing one needs it.
            let (tx, rx) = mpsc::channel();
            let p = path.clone();
            std::thread::spawn(move || {
                let _ = tx.send(std::panic::catch_unwind(|| read_everything(&p)).is_ok());
            });

            match rx.recv_timeout(Duration::from_secs(60)) {
                Ok(true) => {
                    let _ = std::fs::remove_file(&broken);
                }
                Ok(false) => panic!("panicked on {name} round {round}, kept at {path}"),
                Err(_) => panic!("still reading after 60s: {name} round {round}, kept at {path}"),
            }
        }
    }
}
