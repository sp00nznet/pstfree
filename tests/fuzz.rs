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

    for name in ["dist-list.pst", "example-2013.ost"] {
        let src = format!("tests/data/{name}");
        if !std::path::Path::new(&src).exists() {
            eprintln!("skipping {name}: run tests/fetch-fixtures.ps1");
            continue;
        }
        let clean = std::fs::read(&src).unwrap();

        for round in 1..=rounds {
            let mut rng = round.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut bytes = clean.clone();

            // A handful of runs rather than scattered single bytes: a bad sector, a torn
            // write or a partial overwrite all lose a contiguous stretch, not a byte here
            // and there. Half are zeroed and half filled with junk, because a run of
            // zeroes reads as a plausible-but-empty structure while junk reads as absurd
            // lengths, and those break parsers differently.
            for _ in 0..1 + next(&mut rng) % 8 {
                // Half the runs land in the first 16KB. Uniform offsets almost never hit
                // it, and that is where every field a parser trusts lives — the header,
                // the B-tree roots, the first index pages. Damage to a mail body is a
                // wrong subject line; damage here is the count someone loops on.
                let reach = if next(&mut rng) & 1 == 0 {
                    bytes.len().min(16 * 1024)
                } else {
                    bytes.len()
                };
                let at = next(&mut rng) as usize % reach;
                let end = (at + 1 + next(&mut rng) as usize % 512).min(bytes.len());
                let zeroed = next(&mut rng) & 1 == 0;
                for b in &mut bytes[at..end] {
                    *b = if zeroed { 0 } else { next(&mut rng) as u8 };
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
