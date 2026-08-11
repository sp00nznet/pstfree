use pstfree::export;
use pstfree::ltp::{
    self, clean_subject, filetime, read_node_pc, read_tc, NID_ROOT_FOLDER, NID_TYPE_CONTENTS_TABLE,
    NID_TYPE_HIERARCHY_TABLE, PID_CONTENT_COUNT, PID_DELIVERY_TIME, PID_DISPLAY_NAME,
    PID_MESSAGE_SIZE, PID_SENDER_NAME, PID_SUBJECT, PID_SUBMIT_TIME, PID_UNREAD_COUNT,
};
use pstfree::ndb::{nid_type_name, Block, Crypt, Node, Pst};
use std::collections::{BTreeMap, BTreeSet};

const NID_TYPE_FOLDER: u8 = 0x02;
const NID_TYPE_MESSAGE: u8 = 0x04;
/// Enough named nodes to go and check, before the list becomes a count again.
const STALE_LISTED: usize = 20;

const USAGE: &str = "\
pstfree - read, export and repair Outlook PST/OST files

  pstfree <file.pst>            what is in this file, and what is wrong with it
  pstfree <file.pst> --tree     the folder tree, with names and message counts
  pstfree <file.pst> --list     every message: date, folder, sender, subject
  pstfree <file.pst> --props <nid>   every property on one node, as stored
  pstfree <file.pst> --export <dir> [--format eml|mbox|msg]   write the mail out
  pstfree <file.pst> --rebuild <out.pst>   write a clean copy with a fresh index
  pstfree <file.pst> --verify   check every checksum, and what a sweep would recover
  pstfree <file.pst> --nodes    every node in the file
  pstfree <file.pst> --blocks   every block in the file

Add --salvage to any command to rebuild the index by sweeping the file for
surviving B-tree pages, instead of following the one in the header.

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

    let salvage = args.iter().any(|a| a == "--salvage");
    let (nodes, blocks) = index(&mut pst, salvage);

    match args.get(1).map(String::as_str) {
        Some("--verify") => verify(&mut pst, &nodes, &blocks),
        Some("--rebuild") => {
            let Some(out) = args.get(2) else {
                eprintln!("--rebuild needs somewhere to write, e.g. --rebuild fixed.pst");
                std::process::exit(2);
            };
            match pstfree::repair::rebuild(&mut pst, &nodes, &blocks, out) {
                Ok(r) => {
                    println!(
                        "Wrote {out}: {} node(s), {} block(s), {} bytes.",
                        r.nodes, r.blocks, r.bytes
                    );
                    if r.dropped_blocks > 0 || r.dropped_nodes > 0 {
                        println!(
                            "  Left out {} block(s) that failed their checksum and {} node(s) \
                             whose data they held.",
                            r.dropped_blocks, r.dropped_nodes
                        );
                    }
                    if r.missing.is_empty() {
                        println!(
                            "  The allocation maps are marked invalid, which is the documented \
                             way to say\n  'rebuild these before writing' — Outlook does that \
                             on open. Reading the\n  new file back will report that one thing, \
                             and it is meant to."
                        );
                    } else {
                        // Handing back a file that silently will not open is the exact
                        // behaviour this project exists to be the opposite of.
                        println!(
                            "\n  This file will NOT open: it has no {}.\n  \
                             That node's data block did not survive, and no index can point \
                             at bytes\n  that are gone. pstfree itself still reads the \
                             result, so --export is the\n  way to get this mail out.",
                            r.missing.join(", no ")
                        );
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        Some("--tree") => tree(&mut pst, &nodes),
        Some("--list") => list(&mut pst, &nodes),
        Some("--props") => props(&mut pst, &nodes, args.get(2).map(String::as_str)),
        Some("--export") => {
            let fmt = match args.iter().position(|a| a == "--format") {
                Some(i) => match args.get(i + 1).and_then(|s| export::Format::parse(s)) {
                    Some(f) => f,
                    None => {
                        eprintln!("--format takes eml, mbox or msg");
                        std::process::exit(2);
                    }
                },
                None => export::Format::Eml,
            };
            run_export(&mut pst, &nodes, args.get(2).map(String::as_str), fmt)
        }
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

/// The node and block indexes, following the header's B-tree roots.
///
/// Falls back to sweeping the file when that produces nothing, because a file whose roots
/// are gone is exactly the file someone is trying to rescue. `--salvage` forces the sweep
/// even when the roots look fine, which is what to reach for when they are intact but
/// wrong.
fn index(pst: &mut Pst, salvage: bool) -> (Vec<Node>, Vec<Block>) {
    let (mut nodes, mut blocks) = if salvage {
        (Vec::new(), Vec::new())
    } else {
        (pst.nodes(), pst.blocks())
    };
    if !nodes.is_empty() && !blocks.is_empty() {
        return (nodes, blocks);
    }

    // Each tree is replaced only if it is the one that failed. The two die separately,
    // and a swept index carries entries freed long ago whose blocks have since been
    // reused - so preferring an intact tree is not tidiness, it is the difference
    // between reporting real damage and reporting ghosts.
    let r = pst.scan();
    if blocks.is_empty() {
        // Carving beats sweeping here. A swept index page can only describe blocks it
        // knew about when it was written, and if the live page is the one that was lost,
        // every surviving copy is out of date - which silently yields an older revision
        // of a message. Carving reads the blocks themselves, so it finds what is actually
        // there. The swept entries fill in anything carving missed.
        let carved = pst.carve();
        let mut merged: BTreeMap<u64, Block> = carved.iter().map(|b| (b.bid & !1, *b)).collect();

        // Swept entries fill in anything carving missed, but only where the bytes they
        // describe still check out. An entry freed long ago points at space that has been
        // reused since, and letting those into the index turns every one of them into a
        // "damaged block" in the report - ghosts of things that were deleted on purpose.
        let mut from_pages = 0;
        for b in &r.blocks {
            if !merged.contains_key(&(b.bid & !1)) && pst.block_intact(b) {
                merged.insert(b.bid & !1, *b);
                from_pages += 1;
            }
        }
        eprintln!(
            "Block index unreadable. Carved {} blocks out of the file itself, plus {from_pages} from surviving index pages.",
            carved.len()
        );
        blocks = merged.into_values().collect();
        blocks.sort_by_key(|b| b.bid);
        pst.adopt(&blocks);
    }
    if nodes.is_empty() {
        eprintln!(
            "Node index unreadable. Swept {} nodes from {} surviving pages.",
            r.nodes.len(),
            r.nbt_pages
        );
        nodes = r.nodes;
        // Only worth saying here. While the file's own node index is readable it settles
        // every one of these, and the sweep's disagreements are ghosts of freed pages
        // rather than findings — warning about them then would be crying wolf over a
        // healthy file. Once the sweep *is* the index, they are the real limit of it.
        report_stale(&r.stale);
    }
    (nodes, blocks)
}

/// Name the nodes whose recovered revision cannot be vouched for.
///
/// A count would be worse than useless here. "Some of your mail may be an old copy" is
/// something nobody can act on; a node id can be looked up with `--props` and the message
/// read to see whether it is the one expected. The list is capped because a thoroughly
/// swept file can produce a lot of them, and a wall of ids is a count again.
fn report_stale(stale: &[pstfree::ndb::Stale]) {
    if stale.is_empty() {
        return;
    }
    let gone = stale.iter().filter(|s| s.dangling).count();
    eprintln!(
        "\n  {} node(s) recovered at a revision that cannot be confirmed{}:",
        stale.len(),
        if gone > 0 {
            format!(", {gone} of them pointing at data that is not in the file")
        } else {
            String::new()
        }
    );
    for s in stale.iter().take(STALE_LISTED) {
        if s.dangling {
            eprintln!(
                "    - node 0x{:X}: block {} is not in the index, so this node's data is gone",
                s.nid, s.bid_data
            );
        } else {
            eprintln!(
                "    - node 0x{:X}: {} revisions survive, took block {} as the newest",
                s.nid, s.versions, s.bid_data
            );
        }
    }
    if stale.len() > STALE_LISTED {
        eprintln!("    ... and {} more", stale.len() - STALE_LISTED);
    }
    eprintln!(
        "  These read normally. If the page holding a node's newest index entry was one of\n  \
         the ones lost, what survives is an older copy and nothing in the file says so."
    );
}

/// Check everything that can be checked, and say what a sweep would find that the index
/// does not. This is the "what is actually wrong with my file" command.
fn verify(pst: &mut Pst, nodes: &[Node], blocks: &[Block]) {
    println!("Reading every block to check it against its own checksum...");
    let (mut ok, mut bad) = (0usize, 0usize);
    let before = pst.problem_count();
    for b in blocks {
        match pst.block(b.bid) {
            Ok(_) => ok += 1,
            Err(_) => bad += 1,
        }
    }
    // A block that read but warned is one whose checksum failed - damaged, not missing.
    let rotten = pst.problem_count().saturating_sub(before);

    let r = pst.scan();
    println!();
    println!(
        "  {} nodes and {} blocks reachable from the header's index",
        nodes.len(),
        blocks.len()
    );
    println!("  {ok} blocks read, {bad} unreadable, {rotten} failed their checksum");
    println!(
        "  {} pages swept, {} of them index pages ({} node, {} block)",
        r.pages_scanned,
        r.nbt_pages + r.bbt_pages,
        r.nbt_pages,
        r.bbt_pages
    );
    // Both of these are normal in a healthy file. A PST frees pages by unlinking them and
    // leaves the bytes where they are, so the sweep keeps finding old ones. Reported so
    // the numbers are not mistaken for damage.
    if r.damaged_pages > 0 {
        println!(
            "  {} index pages failed their checksum — usually freed pages partly overwritten",
            r.damaged_pages
        );
    }
    if r.superseded > 0 {
        println!(
            "  {} superseded entries ignored — older copies of things that later moved",
            r.superseded
        );
    }
    if r.unresolved > 0 {
        println!(
            "  {} of those could not be confirmed against the bytes on disk — least trustworthy",
            r.unresolved
        );
    }
    let carved = pst.carve().len();
    println!("  {carved} blocks can be carved out of the file with no index at all");

    // A second, independent record of what is in each folder. The parent pointers say
    // one thing and the folder's own tables say another, and in an undamaged file they
    // agree exactly - so where they do not, something is wrong that nothing else catches.
    let by_nid: BTreeMap<u32, Node> = nodes.iter().map(|n| (n.nid, *n)).collect();
    let (mut compared, mut disagree) = (0usize, 0usize);
    for f in nodes.iter().filter(|n| n.nid_type() == NID_TYPE_FOLDER) {
        for (table_type, want) in [
            (NID_TYPE_HIERARCHY_TABLE, [0x02u8, 0x03].as_slice()),
            (NID_TYPE_CONTENTS_TABLE, [0x04].as_slice()),
        ] {
            let Some(t) = by_nid.get(&((f.nid & !0x1F) | table_type)) else {
                continue;
            };
            let Ok(tc) = read_tc(pst, t.bid_data, t.bid_sub) else {
                continue;
            };
            let listed: BTreeSet<u32> = tc.rows.iter().map(|r| r.id).collect();
            let pointed: BTreeSet<u32> = nodes
                .iter()
                .filter(|n| want.contains(&n.nid_type()) && n.nid_parent == f.nid && n.nid != f.nid)
                .map(|n| n.nid)
                .collect();
            compared += 1;
            if listed != pointed {
                disagree += 1;
                let only_table = listed.difference(&pointed).count();
                let only_ptr = pointed.difference(&listed).count();
                println!(
                    "  folder 0x{:X}: its own table and the node parents disagree — {only_table} listed only in the table, {only_ptr} only by parent",
                    f.nid
                );
            }
        }
    }
    println!(
        "  {compared} folder tables cross-checked against the node parents, {disagree} disagreed"
    );

    // The number that matters: what a sweep gets back that the index has lost.
    let known: std::collections::HashSet<u32> = nodes.iter().map(|n| n.nid).collect();
    let extra: Vec<&Node> = r.nodes.iter().filter(|n| !known.contains(&n.nid)).collect();
    println!();
    if extra.is_empty() {
        println!("  Sweeping finds nothing the index has lost. The two agree.");
    } else {
        let msgs = extra
            .iter()
            .filter(|n| n.nid_type() == NID_TYPE_MESSAGE)
            .count();
        let folders = extra
            .iter()
            .filter(|n| n.nid_type() == NID_TYPE_FOLDER)
            .count();
        println!(
            "  Sweeping recovers {} node(s) the index cannot reach — {msgs} message(s), {folders} folder(s).",
            extra.len()
        );
        println!("  Run any command with --salvage to use them.");
    }

    if pst.warnings.is_empty() {
        println!("\n  No damage found.");
    } else {
        println!("\n  {} problem(s):", pst.problem_count());
        for w in &pst.warnings {
            println!("    - {w}");
        }
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

    // Newest first, and the timestamp is a plain u64, so reversing the key beats comparing
    // backwards by hand.
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    println!("{:<16}  {:<22}  {:<20}  subject", "date", "folder", "from");
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

fn run_export(pst: &mut Pst, nodes: &[Node], dir: Option<&str>, format: export::Format) {
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
    let st = export::export(pst, nodes, &names, root, format);

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

    let node = *node;
    // Ids from 0x8000 up are numbered per file rather than by any specification, so the
    // number alone says nothing. The file's own map says what they stand for.
    let named = ltp::read_names(pst, nodes);

    match read_node_pc(pst, &node) {
        Err(e) => eprintln!("0x{nid:X}: {e}"),
        Ok(pc) => {
            println!(
                "node 0x{nid:X}, {}, {} properties, {} fetched from the subnode tree",
                nid_type_name(node.nid_type()),
                pc.props.len(),
                pc.from_subnode
            );
            for (id, v) in &pc.props {
                match named.get(*id) {
                    Some(n) => println!("  0x{id:04X}  {}  {n}", describe(v)),
                    None => println!("  0x{id:04X}  {}", describe(v)),
                }
            }
            if pc.props.keys().any(|id| *id >= 0x8000) && named.is_empty() {
                println!(
                    "\n  This file's name-to-id map (node 0x61) could not be read, so the \
                     properties\n  at 0x8000 and above can only be shown by number."
                );
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

fn summary(path: &str, pst: &Pst, nodes: &[Node], blocks: &[Block]) {
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
