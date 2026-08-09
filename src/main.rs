mod ndb;

use ndb::{nid_type_name, Crypt, Pst};
use std::collections::BTreeMap;

const USAGE: &str = "\
pstfree - read, export and repair Outlook PST/OST files

  pstfree <file.pst>            what is in this file, and what is wrong with it
  pstfree <file.pst> --nodes    every node in the file
  pstfree <file.pst> --blocks   every block in the file

No Outlook needed, and no password is ever asked for - a PST password is a
checksum, not a key, and it protects nothing.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = match args.first() {
        Some(a) if !a.starts_with('-') => a.clone(),
        _ => {
            print!("{USAGE}");
            std::process::exit(2);
        }
    };

    let mut pst = match Pst::open(&path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let nodes = pst.nodes();
    let blocks = pst.blocks();

    match args.get(1).map(String::as_str) {
        Some("--nodes") => {
            println!("{:>10}  {:<26} {:>10}  {:>18} {:>18}", "nid", "type", "parent", "data block", "sub block");
            for n in &nodes {
                println!(
                    "{:>10}  {:<26} {:>10}  {:>18} {:>18}",
                    format!("0x{:X}", n.nid),
                    nid_type_name(n.nid_type()),
                    format!("0x{:X}", n.nid_parent),
                    n.bid_data,
                    n.bid_sub
                );
            }
        }
        Some("--blocks") => {
            println!("{:>18} {:>14} {:>8} {:>6}", "bid", "offset", "bytes", "refs");
            for b in &blocks {
                println!("{:>18} {:>14} {:>8} {:>6}", b.bid, b.ib, b.cb, b.cref);
            }
        }
        Some(other) => {
            eprintln!("unknown option {other}\n\n{USAGE}");
            std::process::exit(2);
        }
        None => summary(&path, &pst, &nodes, &blocks),
    }
}

fn summary(path: &str, pst: &Pst, nodes: &[ndb::Node], blocks: &[ndb::Block]) {
    // From the header, not the file name — a renamed file still tells the truth.
    let kind = if pst.is_ost { "OST" } else { "PST" };
    let pages = if pst.ver >= 36 { "4K pages" } else { "512-byte pages" };
    println!("{path}");
    println!("  {kind}, Unicode format (version {}, {pages})", pst.ver);
    println!(
        "  {} bytes on disk, {} declared in the header",
        pst.actual_len, pst.declared_len
    );
    println!(
        "  block encoding: {}",
        match pst.crypt {
            Crypt::None => "none".to_string(),
            Crypt::Permute => "permute - a fixed substitution table, no key".to_string(),
            Crypt::Cyclic => "cyclic - a fixed table keyed off the block id, not a password".to_string(),
            Crypt::Unknown(b) => format!("unrecognised (0x{b:02X})"),
        }
    );

    let mut counts: BTreeMap<u8, usize> = BTreeMap::new();
    for n in nodes {
        *counts.entry(n.nid_type()).or_default() += 1;
    }
    let bytes: u64 = blocks.iter().map(|b| b.cb as u64).sum();

    println!("\n  {} nodes, {} blocks, {bytes} bytes of block data", nodes.len(), blocks.len());
    for (t, c) in &counts {
        println!("    {c:>7}  0x{t:02X}  {}", nid_type_name(*t));
    }

    if pst.warnings.is_empty() {
        println!("\n  No structural damage found.");
    } else {
        println!("\n  {} problem(s) found:", pst.warnings.len());
        for w in &pst.warnings {
            println!("    - {w}");
        }
    }
}
