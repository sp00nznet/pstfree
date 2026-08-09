mod crypt;
mod export;
mod ltp;
mod ndb;

use ltp::{
    clean_subject, filetime, read_node_pc, NID_ROOT_FOLDER, PID_CONTENT_COUNT, PID_DELIVERY_TIME,
    PID_DISPLAY_NAME, PID_MESSAGE_SIZE, PID_SENDER_NAME, PID_SUBJECT, PID_SUBMIT_TIME,
    PID_UNREAD_COUNT,
};
use ndb::{nid_type_name, Crypt, Node, Pst};
use std::collections::BTreeMap;

const NID_TYPE_FOLDER: u8 = 0x02;
const NID_TYPE_MESSAGE: u8 = 0x04;

const USAGE: &str = "\
pstfree - read, export and repair Outlook PST/OST files

  pstfree <file.pst>            what is in this file, and what is wrong with it
  pstfree <file.pst> --tree     the folder tree, with names and message counts
  pstfree <file.pst> --list     every message: date, folder, sender, subject
  pstfree <file.pst> --props <nid>   every property on one node, as stored
  pstfree <file.pst> --export <dir>  write every message out as a .eml file
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
        Some("--list") => list(&mut pst, &nodes),
        Some("--props") => props(&mut pst, &nodes, args.get(2).map(String::as_str)),
        Some("--export") => run_export(&mut pst, &nodes, args.get(2).map(String::as_str)),
        Some("--nodes") => {
            println!(
                "{:>10}  {:<26} {:>10}  {:>18} {:>18}",
                "nid", "type", "parent", "data block", "sub block"
            );
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
            println!(
                "{:>18} {:>14} {:>8} {:>6}",
                "bid", "offset", "bytes", "refs"
            );
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

/// Name and message counts for every folder, read once.
fn folders(pst: &mut Pst, nodes: &[Node]) -> (BTreeMap<u32, String>, BTreeMap<u32, String>, usize) {
    let mut plain = BTreeMap::new();
    let mut labelled = BTreeMap::new();
    let mut unreadable = 0;

    for n in nodes.iter().filter(|n| n.nid_type() == NID_TYPE_FOLDER) {
        match read_node_pc(pst, n) {
            Ok(pc) => {
                // The root folder carries no display name in any file; it is the anchor
                // the rest hangs off, not something Outlook ever shows.
                let fallback = if n.nid == NID_ROOT_FOLDER {
                    "(root)"
                } else {
                    "(unnamed)"
                };
                // An empty display name is as good as no display name, and a folder named
                // "" would otherwise become a directory with no name at all.
                let name = pc
                    .str(PID_DISPLAY_NAME)
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(fallback)
                    .to_string();
                let count = pc.int(PID_CONTENT_COUNT).unwrap_or(0);
                let unread = pc.int(PID_UNREAD_COUNT).unwrap_or(0);
                labelled.insert(
                    n.nid,
                    match (count, unread) {
                        (0, _) => name.clone(),
                        (c, 0) => format!("{name}  ({c})"),
                        (c, u) => format!("{name}  ({c}, {u} unread)"),
                    },
                );
                plain.insert(n.nid, name);
            }
            Err(e) => {
                unreadable += 1;
                plain.insert(n.nid, "(unreadable)".into());
                labelled.insert(n.nid, format!("(unreadable: {e})"));
            }
        }
    }
    (plain, labelled, unreadable)
}

/// Every message in the file, newest first, with the folder it sits in.
fn list(pst: &mut Pst, nodes: &[Node]) {
    let (folder, _, _) = folders(pst, nodes);
    let mut rows = Vec::new();
    let mut unreadable = 0;

    for n in nodes.iter().filter(|n| n.nid_type() == NID_TYPE_MESSAGE) {
        match read_node_pc(pst, n) {
            Ok(pc) => {
                let when = pc
                    .time(PID_DELIVERY_TIME)
                    .or(pc.time(PID_SUBMIT_TIME))
                    .unwrap_or(0);
                rows.push((
                    when,
                    folder
                        .get(&n.nid_parent)
                        .cloned()
                        .unwrap_or_else(|| "(no folder)".into()),
                    pc.str(PID_SENDER_NAME).unwrap_or("").to_string(),
                    clean_subject(pc.str(PID_SUBJECT).unwrap_or("(no subject)")).to_string(),
                    pc.int(PID_MESSAGE_SIZE).unwrap_or(0),
                ));
            }
            Err(e) => {
                unreadable += 1;
                eprintln!("message 0x{:X}: {e}", n.nid);
            }
        }
    }

    rows.sort_by(|a, b| b.0.cmp(&a.0));
    println!(
        "{:<16}  {:<22}  {:<20}  {}",
        "date", "folder", "from", "subject"
    );
    for (when, folder, from, subject, size) in &rows {
        println!(
            "{}  {:<22}  {:<20}  {subject}{}",
            filetime(*when),
            trim(folder, 22),
            trim(from, 20),
            if *size > 0 {
                format!("  [{size} bytes]")
            } else {
                String::new()
            }
        );
    }
    println!("\n{} message(s).", rows.len());
    if unreadable > 0 {
        println!("{unreadable} could not be read; see above.");
    }
}

fn run_export(pst: &mut Pst, nodes: &[Node], dir: Option<&str>) {
    let Some(dir) = dir else {
        eprintln!("--export needs a directory to write into, e.g. --export .\\mail");
        std::process::exit(2);
    };
    let root = std::path::Path::new(dir);
    // Refusing to write into a directory that already has things in it: an export is
    // hundreds of files and merging it into someone's existing folder is not undoable.
    if root.read_dir().is_ok_and(|mut d| d.next().is_some()) {
        eprintln!("{dir} already exists and is not empty. Give me a new directory.");
        std::process::exit(1);
    }

    let (names, _, _) = folders(pst, nodes);
    let st = export::export(pst, nodes, &names, root);

    println!("{} message(s) written to {dir}", st.messages);
    if st.attachments > 0 {
        println!("{} attachment(s) included.", st.attachments);
    }
    if st.failed > 0 {
        println!("\n{} message(s) could not be written:", st.failed);
        for e in &st.errors {
            println!("  - {e}");
        }
    }
    if !pst.warnings.is_empty() {
        println!("\n{} problem(s) in the file itself:", pst.warnings.len());
        for w in &pst.warnings {
            println!("  - {w}");
        }
    }
}

/// Every property on one node, exactly as stored. The answer to "what is actually in
/// this thing", which is the question a damaged file always raises.
fn props(pst: &mut Pst, nodes: &[Node], want: Option<&str>) {
    let Some(nid) = want.and_then(|s| {
        let s = s.trim_start_matches("0x");
        u32::from_str_radix(s, 16).ok()
    }) else {
        eprintln!("--props needs a node id in hex, as printed by --nodes, e.g. --props 200044");
        std::process::exit(2);
    };
    let Some(node) = nodes.iter().find(|n| n.nid == nid) else {
        eprintln!("no node 0x{nid:X} in this file");
        std::process::exit(1);
    };

    match read_node_pc(pst, node) {
        Err(e) => eprintln!("0x{nid:X}: {e}"),
        Ok(pc) => {
            println!(
                "node 0x{nid:X}, {}, {} properties, {} fetched from the subnode tree",
                nid_type_name(node.nid_type()),
                pc.props.len(),
                pc.from_subnode
            );
            for (id, v) in &pc.props {
                println!("  0x{id:04X}  {}", describe(v));
            }
        }
    }
}

fn describe(v: &ltp::Value) -> String {
    use ltp::Value::*;
    match v {
        Int(i) => format!("int          {i}"),
        Float(f) => format!("float        {f}"),
        Bool(b) => format!("bool         {b}"),
        Time(t) => format!("time         {}", filetime(*t)),
        Str(s) => format!("string       {:?}", trim(s, 60)),
        Bytes(b) => format!("binary       {} bytes", b.len()),
        MissingSubnode(n) => format!("MISSING      subnode 0x{n:08X} is not in the subnode tree"),
        Raw { ptype, bytes } => format!("type 0x{ptype:04X}   {} bytes", bytes.len()),
    }
}

fn trim(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n - 1).collect::<String>() + "…"
}

/// The folder tree, built from the node B-tree's own parent pointers.
///
/// ponytail: every node records its parent, so both the hierarchy here and each message's
/// containing folder in `list` fall straight out of the node list, and the folders' own
/// hierarchy and contents tables are never touched. Reading those means implementing
/// table contexts, which is real work and buys nothing yet. They become worth doing for
/// attachments, and for a file damaged badly enough that the parent pointers and the
/// tables disagree - at which point having both is exactly the point.
fn tree(pst: &mut Pst, nodes: &[Node]) {
    let (_, name, unreadable) = folders(pst, nodes);
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for n in nodes.iter().filter(|n| n.nid_type() == NID_TYPE_FOLDER) {
        children.entry(n.nid_parent).or_default().push(n.nid);
    }

    if name.is_empty() {
        println!("No folders found.");
        return;
    }

    // Print from the root, then anything the root cannot reach - a folder whose parent
    // is missing is exactly what a damaged file looks like, and it must not vanish.
    let mut shown = std::collections::HashSet::new();
    print_branch(NID_ROOT_FOLDER, 0, &name, &children, &mut shown);
    let orphans: Vec<u32> = name
        .keys()
        .copied()
        .filter(|n| !shown.contains(n))
        .collect();
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
    let pages = if pst.ver >= 36 {
        "4K pages"
    } else {
        "512-byte pages"
    };
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
            Crypt::Cyclic =>
                "cyclic - a fixed table keyed off the block id, not a password".to_string(),
            Crypt::Unknown(b) => format!("unrecognised (0x{b:02X})"),
        }
    );

    let mut counts: BTreeMap<u8, usize> = BTreeMap::new();
    for n in nodes {
        *counts.entry(n.nid_type()).or_default() += 1;
    }
    let bytes: u64 = blocks.iter().map(|b| b.cb as u64).sum();

    println!(
        "\n  {} nodes, {} blocks, {bytes} bytes of block data",
        nodes.len(),
        blocks.len()
    );
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
