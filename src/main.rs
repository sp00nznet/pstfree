mod crypt;
mod ltp;
mod ndb;

use ltp::{Heap, Pc, PID_CONTENT_COUNT, PID_DISPLAY_NAME, PID_UNREAD_COUNT};
use ndb::{nid_type_name, Crypt, Node, Pst};
use std::collections::BTreeMap;

/// NID of the root folder. Fixed by the specification, the same in every file.
const NID_ROOT_FOLDER: u32 = 0x122;
const NID_TYPE_FOLDER: u8 = 0x02;

const USAGE: &str = "\
pstfree - read, export and repair Outlook PST/OST files

  pstfree <file.pst>            what is in this file, and what is wrong with it
  pstfree <file.pst> --tree     the folder tree, with names and message counts
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
        Some("--tree") => tree(&mut pst, &nodes),
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

/// The folder tree, built from the node B-tree's own parent pointers.
///
/// ponytail: every node records its parent, so the hierarchy falls straight out of the
/// node list and the folders' own hierarchy tables are never touched. Reading those means
/// implementing table contexts, which is real work and buys nothing here. It becomes
/// worth doing for message listings, and for a file damaged badly enough that the parent
/// pointers disagree with the tables - at which point having both is the point.
fn tree(pst: &mut Pst, nodes: &[Node]) {
    let mut name = BTreeMap::new();
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut unreadable = 0;

    for n in nodes.iter().filter(|n| n.nid_type() == NID_TYPE_FOLDER) {
        children.entry(n.nid_parent).or_default().push(n.nid);
        let pc = pst.node_blocks(n.bid_data).and_then(Heap::new).and_then(|h| Pc::read(&h));
        name.insert(
            n.nid,
            match pc {
                Ok(pc) => {
                    // The root folder carries no display name in any file; it is the
                    // anchor the rest hangs off, not something Outlook ever shows.
                    let fallback =
                        if n.nid == NID_ROOT_FOLDER { "(root)" } else { "(unnamed)" };
                    let label = pc.str(PID_DISPLAY_NAME).unwrap_or(fallback).to_string();
                    let count = pc.int(PID_CONTENT_COUNT).unwrap_or(0);
                    let unread = pc.int(PID_UNREAD_COUNT).unwrap_or(0);
                    match (count, unread) {
                        (0, _) => label,
                        (c, 0) => format!("{label}  ({c})"),
                        (c, u) => format!("{label}  ({c}, {u} unread)"),
                    }
                }
                Err(e) => {
                    unreadable += 1;
                    format!("(unreadable: {e})")
                }
            },
        );
    }

    if name.is_empty() {
        println!("No folders found.");
        return;
    }

    // Print from the root, then anything the root cannot reach - a folder whose parent
    // is missing is exactly what a damaged file looks like, and it must not vanish.
    let mut shown = std::collections::HashSet::new();
    print_branch(NID_ROOT_FOLDER, 0, &name, &children, &mut shown);
    let orphans: Vec<u32> = name.keys().copied().filter(|n| !shown.contains(n)).collect();
    if !orphans.is_empty() {
        println!("\n{} folder(s) not reachable from the root:", orphans.len());
        for o in orphans {
            print_branch(o, 1, &name, &children, &mut shown);
        }
    }
    if unreadable > 0 {
        println!("\n{unreadable} folder(s) could not be read.");
    }
}

fn print_branch(
    nid: u32,
    depth: usize,
    name: &BTreeMap<u32, String>,
    children: &BTreeMap<u32, Vec<u32>>,
    shown: &mut std::collections::HashSet<u32>,
) {
    if !shown.insert(nid) {
        return; // a cycle in the parent pointers, which a damaged file can produce
    }
    if let Some(label) = name.get(&nid) {
        println!("{}{label}", "  ".repeat(depth));
    }
    for c in children.get(&nid).map(Vec::as_slice).unwrap_or(&[]) {
        print_branch(*c, depth + 1, name, children, shown);
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

