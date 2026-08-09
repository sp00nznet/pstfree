# pstfree

Read, export and repair Outlook `.pst` and `.ost` files on Windows. No Outlook
required, no licence, no per-mailbox fee, no fake progress bar. MIT licensed.

**Status: it gets your mail out, and it does not need the file to be intact.** Folder tree,
message list, every property on any node, and `--export` to `.eml` files any mail client
will open — from PST and OST alike, including from a password-protected PST without ever
asking for the password. **Delete both of a PST's B-tree roots and it still recovers every
message, byte for byte.** See [Progress](#progress).

Sibling project to [vncfree](https://github.com/sp00nznet/vncfree), same attitude:
find the Windows payware, read the published spec it is hiding behind, give it away.

## Why

There is an entire industry selling PST tools for $50–$300. "PST Repair." "OST to PST
Converter." "PST Password Recovery." Kernel, Stellar, SysTools, DataNumen, Aryson —
dozens of near-identical products, all with the same SEO blogspam, the same three-pane
screenshot, the same free trial that shows you your own email and then asks for money
before it will save it.

Here is what they are selling:

- **The file format is published by Microsoft.** [MS-PST] is a public, versioned,
  several-hundred-page specification. Not reverse-engineered. Not leaked. Downloadable.
  It describes the node database, the B-trees, the property layer and the encoding —
  everything a reader needs. OST is the same container.
- **The "password" is not encryption.** A PST password is stored as a **CRC32 of the
  password** in the message store. The block obfuscation uses a **fixed table with no key
  at all** — identical whether the file has a password or not. MS-PST section 5 calls the
  two ciphers "**keyless**" in as many words. Any reader can simply not ask. The products
  charging $30–$50 to "recover" a PST password are charging you to skip an `if` statement.
  pstfree reads a password-protected file below and never asks; there is no code in it
  that could.
- **Repair is the only genuinely hard part** — and it is the part the marketing is
  vaguest about.

So the honest summary of the category is: a documented parser, a progress bar, and a
paywall placed between you and email you already own.

**If anyone tries to sell you this software, they're scamming you. Walk away.**

## The clock that makes this worth building

PST is being retired, and *that is the argument for the tool, not against it.*

| When | What happens |
|---|---|
| **April 2026** | Classic Outlook enters its opt-out phase — new Outlook becomes the default |
| **March 2027** | Enterprise customers stop being able to defer the move |
| **Q2 2029** | Classic Outlook support ends entirely — updates, fixes, security patches |

New Outlook is a web client. It does not mount PST files as folders; it *imports* from
them, and even that has needed classic Outlook installed alongside. Microsoft has said it
has no plans to keep developing PST.

The consequence: twenty-plus years of archives, legal holds, ex-employee mailboxes and
"I'll deal with that later" `.ost` files are about to have **no first-party reader at
all**. `scanpst.exe`, the free Inbox Repair Tool, ships with classic Outlook — so the
only free repair option disappears on the same schedule.

That is the moment the $300 tools are waiting for. Better if something free is standing
there instead.

## Prior art, credit where due

The starting question for this repo was: *[hrbrmstr/freepst] exists — can we do better,
or should we not bother?*

Answer: freepst isn't the bar. It's an [rJava] wrapper around [java-libpst], 5 stars,
untouched since 2020. It needs R **and** a JVM, it exposes folders and messages as data
frames, and it is aimed squarely at data scientists who want a mailbox in a tibble. It's
a fine thing to be. It is not a Windows tool and was never trying to be one.

The bar is these:

| Project | What it is | State |
|---|---|---|
| **[libpff]** (libyal) | C library + `pffexport`. The serious one. Deepest format coverage anywhere, including recovery of deleted items. LGPL-3.0 | 361★, active — pushed June 2026 |
| **[XstReader]** | C# WPF viewer for PST/OST. Closest existing free *Windows GUI*. MS-PL | 680★, last pushed Sept 2023 |
| **[java-libpst]** | Pure-Java reader. What freepst wraps | 273★, 2022 |
| **[libpst]** / `readpst` | The old Linux `pst-utils`. PST → mbox | 48★, still maintained |
| **[freepst]** | rJava wrapper around java-libpst, for R | 5★, 2020 |

Nobody in that list is a bad project and none of this is a complaint about their code.
The gap is not "can a PST be parsed" — it was solved years ago. The gap is everywhere
else:

- **Nothing there is a drop-on-a-machine Windows executable.** libpff is a build; XstReader
  is the closest and is a viewer, not an exporter or a repairer, and has been quiet since
  2023.
- **Nothing free seriously attempts repair.** libpff recovers deleted items from an intact
  file — genuinely impressive, and a different problem from a file whose B-tree is torn.
  When `scanpst.exe` gives up, the free world currently gives up too. **This is the entire
  reason the payware has a business.**
- **Orphaned OST is the #1 thing money changes hands over** — the mailbox is gone, the
  employee left, the `.ost` is all that survives, and Outlook won't open an OST it doesn't
  own. Every vendor on that list sells "OST to PST" as a separate SKU.
- **Nobody makes the licence point.** libpff is LGPL, libpst is GPL. Fine licences, but a
  vendor cannot quietly wrap them. MIT means the free thing can go anywhere the paid thing
  goes.

### So: can we do more than freepst?

Yes, but that is the wrong bar and beating it proves nothing. The bar is **libpff for
correctness and XstReader for reach**, and the ground that is actually unclaimed is:

> A single self-contained Windows executable that opens a broken, orphaned, or
> "password-protected" PST/OST with no Outlook installed, shows you what's in it, and
> writes it back out as something you can open — and does not give up where `scanpst.exe`
> does.

That's the product. Everything else is table stakes that already exists for free.

## Try it

```
cargo build --release
target\release\pstfree.exe archive.pst --tree
```

One executable, no runtime, no installer, and it never asks for a password.

**This is a password-protected PST.** No password was set, supplied or requested:

```
> pstfree.exe passworded.pst --tree
(root)
  Top of Personal Folders
    Deleted Items
    Inbox
    Outbox
    Sent Items
    Calendar
    Contacts  (2)
    ...
  Search Root
  Freebusy Data  (1)
```

Without `--tree` it surveys the file instead:

```
> pstfree.exe passworded.pst
  PST, Unicode format (version 23, 512-byte pages)
  271360 bytes on disk, 271360 declared in the header
  block encoding: permute - a fixed substitution table, no key

  130 nodes, 138 blocks, 52912 bytes of block data
          9  0x01  internal
         18  0x02  folder
          3  0x04  message
         13  0x08  associated message
         ...

  No structural damage found.
```

`--list` is every message in the file, newest first:

```
> pstfree.exe mailbox.ost --list
date              folder                  from                  subject
2014-06-05 16:22  Inbox                   Microsoft Outlook     Microsoft Outlook Test Message  [7110 bytes]
2014-04-09 19:54  Inbox                   Microsoft Outlook     Microsoft Outlook Test Message  [7016 bytes]
2014-04-09 16:38  Sent Items              Bernard Chung         Test 2  [12390 bytes]
```

`--props <nid>` is every property on one node, exactly as stored — the answer to "what is
actually in this thing", which is the question a damaged file always raises:

```
> pstfree.exe mailbox.ost --props 200184
node 0x200184, message, 69 properties, 2 fetched from the subnode tree
  0x0037  string       "Test 2"
  0x0039  time         2014-04-09 16:38
  0x007D  string       "Return-Path: <someone@example.com>\r\nDelivered-To…"
  0x0C1A  string       "Bernard Chung"
  0x1013  binary       5816 bytes
  ...
```

### Getting the mail out

```
> pstfree.exe mailbox.ost --export .\mail
3 message(s) written to .\mail

.\mail\Root - Mailbox\IPM_SUBTREE\Inbox\Microsoft Outlook Test Message.eml
.\mail\Root - Mailbox\IPM_SUBTREE\Inbox\Microsoft Outlook Test Message (0x2000E4).eml
.\mail\Root - Mailbox\IPM_SUBTREE\Sent Items\Test 2.eml
```

One directory per folder, one `.eml` per message — RFC 5322 with MIME, which Thunderbird,
Outlook, `mutt` and everything else will open. Attachments are included as MIME parts.

**Where the message carries its original internet headers, those are what gets written** —
the real `From`, the real `Message-ID`, the full `Received` chain, exactly as it arrived.
Only the headers describing the old body layout are replaced, because the body is being
re-encoded. Where a message has no such headers, they are rebuilt from its properties.

Two rules run through the exporter:

- **Nothing is invented.** No timezone is claimed that the file does not record, so dates
  are written `+0000`. A body's character set is declared as the message itself declares
  it and passed through rather than transcoded on a guess, because re-encoding on a guess
  is how mojibake gets baked in permanently.
- **Nothing stops.** One unreadable message does not end the export — it is counted,
  named, and the rest are written. The file that needs exporting is the broken one.

A message whose folder is missing is written to `_no-folder` rather than skipped: in a
damaged file that is the one most worth keeping. Two messages sharing a subject in one
folder get the node id appended, because silently overwriting the first one would destroy
mail.

`--nodes` and `--blocks` dump the two B-trees entry by entry.

### When the index is gone

This is the part the money is charged for, and the part no free tool does.

A PST dies by losing its index. One bad page high in a B-tree orphans everything beneath
it, and `scanpst.exe` — the free Inbox Repair Tool, which leaves with classic Outlook in
2029 — gives up. So does every other free option.

Here is a 271,360-byte PST with **both** B-tree root pages overwritten with zeroes, which
is as dead as `scanpst` can describe:

```
> pstfree.exe torn.pst --list
Block index unreadable. Carved 184 blocks out of the file itself, plus 0 from surviving index pages.
Node index unreadable. Swept 128 nodes from 27 surviving pages.

date              folder        from        subject
2016-08-02 00:27  Calendar      Unknown     Test appointment  [22533 bytes]
2014-05-25 13:58  Contacts      Unknown     test dist list  [1164 bytes]
2014-05-25 13:58  Contacts      Unknown     contact name 1  [953 bytes]
```

Every message, every folder, the same 128 nodes pointing at the same data as the intact
file. `--export` then writes them all out. There is a test that asserts exactly this: tear
the roots out of a real PST and compare the recovered node table against the original,
entry for entry.

It works because nothing in a PST needs the index to be found:

- **Index pages are checksummed and sit at fixed offsets.** Sweeping the file finds every
  surviving B-tree leaf without following a single pointer, and a page that matches its own
  checksum is a page rather than a coincidence.
- **Blocks are checksummed too, and carry their own length and id in a trailer at a fixed
  distance from an aligned boundary.** So the block index can be rebuilt from the *blocks*,
  with no index involved anywhere. That is the last resort, and the strongest one — it
  cannot be fooled into inventing a block, because a candidate is only accepted when the
  checksum matches the bytes in front of it.

**Recovery has to choose between old and new copies**, because a PST frees pages by
unlinking them and leaving the bytes alone — so a sweep keeps finding superseded entries.
Two rules settle it, and both were arrived at by checking the answer against the same file
undamaged:

1. A block index entry wins only if the bytes at its offset still identify as that block
   and match its checksum. Freed entries point at reused space and fail.
2. Between node entries, the highest block id wins. A PST hands out block ids from a
   counter that only goes up, so the largest is the most recently written — a far better
   signal than file position, which only says where the allocator found room. Choosing on
   position instead silently returned a **15,096-byte** older revision of a message whose
   real size is **22,533**. That is the difference between recovering your mail and
   recovering something that used to be your mail.

`--verify` reads every checksum in the file and says what a sweep would recover that the
index cannot reach. `--salvage` on any command ignores the header's index and rebuilds,
for when it is intact but wrong.

### It does not stop at the first bad byte

Cut 40% off the end of a 271,360-byte PST and it recovers **the same 128 nodes and 155
blocks as the intact file**, and says exactly what is gone:

```
  162817 bytes on disk, 271360 declared in the header
  128 nodes, 155 blocks, 73619 bytes of block data

  1 problem(s) found:
    - file is truncated: header says 271360 bytes, 162817 present (108543 missing)
```

The index survives because it lives near the front. So the structure of a mangled mailbox
is completely recoverable even when the mail in the missing region is not — you learn what
was in the file and precisely what is unrecoverable, instead of a percentage and an
invoice. Everything below the header is treated as suspect: bad pages are reported and
walked around, loops and runaway depth are cut off, and a page whose id doesn't match the
index that pointed at it is caught rather than parsed.

## Scope

**Reading** — Unicode and ANSI PST, and OST. Folder tree, messages, properties,
attachments, embedded messages, plain/HTML/compressed-RTF bodies, calendar and contacts.
Password ignored, because there is nothing there to ignore.

**Export** — `.eml` per message with attachments inline, mirroring the folder tree.
`.mbox` and `.msg` to follow, and rebuilding a clean `.pst` from a damaged one.

**Repair** — the differentiator, and the only genuinely hard part. All four now work:

1. Header and B-tree validation with a plain-language report of what's actually wrong.
2. Rebuild the node and block B-trees from surviving pages when the index is torn.
3. Carve orphaned blocks — walk the file for anything that looks like a valid node and
   reattach what can be reattached.
4. Give a straight answer when data is gone, instead of a 100% progress bar and a bill.

**Not in scope** — writing to a live Outlook profile, MAPI, Exchange, and new Outlook's
own undocumented local store. Reading a file is a different job from being a mail client.

## Open questions

Honest list of what hasn't been checked yet. These get answered before any of them get
promised.

- **Attachment extraction has never seen a real attachment.** Not one of the three
  fixtures has any, so the code that pulls them out of the subnode tree is written and the
  MIME assembly around it is unit-tested, but the extraction itself has never run against
  real data. Treat it as unproven until a file with attachments turns up.
- **Recipients come from the display-name properties, not the recipient table.** So `To:`
  and `Cc:` on a rebuilt header carry the names Outlook showed rather than the addresses.
  Messages that kept their original internet headers — most received mail — are unaffected,
  because those are written through verbatim. Fixing it properly means table contexts.
- **`PidTagBody` (`0x1000`) is absent from all three fixtures.** The HTML body
  (`PidTagHtml`, `0x1013`) is there and correct — it scales with the message, 114 bytes
  for a short one and 5816 for a long one. But the plain-text body turns up under
  `0x6619`, which MS-OXPROPS assigns to something else entirely. Property ids either side
  of it decode correctly, so this is not a misaligned read; these files genuinely put it
  there. Export needs this settled, and it wants more than three sample files first.
- **Named properties (`0x8000` and up) are shown by number, not by name.** Resolving them
  means reading the name-to-id map in node `0x61`. They are visible and intact in
  `--props`; they just have no labels yet.
- **`NDB_CRYPT_CYCLIC` is implemented but has never decoded a real file.** No fixture uses
  it. The specification calls it a symmetric cipher and the test checks that running it
  twice returns the original bytes, which is the only evidence behind it. Permute is
  verified against real files and can be trusted; cyclic cannot be, yet.
- **NID types `0x14`–`0x19` are not in MS-PST**, which lists them as unallocated. The test
  OST is full of them — 40 of type `0x14` and 39 of `0x15` in a file with 40 folders, so
  roughly one of each per folder. Best guess is the sync engine's per-folder state, which
  would be OST-only. Currently labelled as undocumented rather than guessed at.
- **Encrypted OST.** Per MS-PST the encoding modes are keyless, but Microsoft 365 profiles
  can restrict a local cache in ways this repo hasn't tested. Needs a real sample.
- **Recovery has only ever been tested against damage this repo inflicted itself.** Zeroed
  root pages and a truncated tail are realistic failures, but they are clean ones. Files
  ruined by a failing disk, a half-finished write or a bad network share fail in messier
  ways, and none of those have been tried.
- **A node whose every surviving index entry is stale recovers as an older revision.**
  Nothing can be done about that — the newer entry is genuinely not in the file any more —
  but the recovery does not currently say which nodes those were, and it should.

### Resolved along the way

- **The checksum is ordinary CRC-32 and did not need transcribing at all.** MS-PST 5.3
  presents it as slicing-by-8 across eight 256-entry tables — eight kilobytes of constants.
  But the first of those tables is the standard CRC-32 table (polynomial `0xEDB88320`), and
  the other seven are an optimisation computing the identical function four bytes at a
  time. So it is generated in ten lines instead, with no initial value and no final
  inversion, and verified against every page and block in three real files.
- **The crypt tables are transcribed from MS-PST 5.1 and checked, not trusted.** The
  specification publishes one 768-byte table in three parts. On the way in: 768 values,
  every one within 0–255, each third a permutation of 0–255, and the third the exact
  inverse of the first. Those properties hold only if every byte is right, so a
  transcription slip could not have survived. The test re-checks all of it.
- **Outlook 2013+ OST files zlib-compress their data blocks, and MS-PST does not mention
  it.** Their block trailer carries eight bytes the specification's does not: a constant,
  and the inflated length. When that disagrees with the stored length, the block is
  zlib — one block in the test file goes 412 bytes → 1178. Found by noticing `78 9c` where
  a heap header should have been.
- **The large-page format differs in three silent ways**, none of which produce an error
  if you assume the 512-byte layout — just plausible rubbish. Trailers sit 24 bytes from
  the end of a page *or block*, not 16; blocks pad to 512 bytes, not 64; B-tree entry
  counts are 16-bit. All three established by reading real files.
- **One dependency, for zlib.** `miniz_oxide`, pure Rust and no build script, so the
  executable stays self-contained. Reading the format itself needs nothing.
- **Rust, parsed from the spec** rather than wrapping libpff, which keeps the licence MIT.
- **ANSI PST is refused, not half-parsed.** Different header layout, 2GB ceiling, Outlook
  97–2002 only. It says so plainly instead of producing wrong answers.
- **The folder tree comes from the node B-tree's parent pointers, not the folders' own
  hierarchy tables.** Every node records its parent, so the tree falls straight out of the
  node list and table contexts are not needed yet. They become worth implementing for
  message listings — and for a file damaged badly enough that the two disagree, at which
  point having both is exactly the point.

## Progress

Updated as things land. Nothing is claimed here until it runs.

| | Milestone | State |
|---|---|---|
| 0 | Repo, scope, prior-art review | ✅ done |
| 1 | Header, node and block B-trees, node survey | ✅ done — PST and OST, both page layouts |
| 4a | The password no-op | ✅ done — it was never asked for |
| 5a | Damage report — truncation, bad pages, loops, wrong ids | ✅ done |
| 2 | Blocks: both ciphers, zlib, heaps and property contexts | ✅ done |
| 3a | The folder tree, with names and message counts | ✅ done |
| 3b | Subnode trees, message properties, `--list` and `--props` | ✅ done |
| 4b | Export — `.eml` with MIME, folder tree, attachments | ✅ done |
| 5b | Every checksum — header, pages, blocks — and `--verify` | ✅ done |
| 6 | Rebuild torn B-trees by sweeping for surviving leaf pages | ✅ done |
| 7 | Carve blocks out of the file with no index at all | ✅ done |
| 3c | Table contexts, for recipients and a second opinion on membership | ⬜ not started |
| 4c | Export to `.mbox` and `.msg` | ⬜ not started |
| 8 | GUI | ⬜ not started |

31 tests, verified against a real PST, a real 2013 OST and a real password-protected PST —
the public fixtures from freepst, fetched by `tests\fetch-fixtures.ps1`. Test files are
not committed, because real PSTs contain real mail; the tests skip rather than fail when
they are absent.

## Licence

MIT. See [LICENSE](LICENSE). Free forever, for everyone. The whole point is that nobody
pays for this.

[MS-PST]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/
[libpff]: https://github.com/libyal/libpff
[XstReader]: https://github.com/Dijji/XstReader
[java-libpst]: https://github.com/rjohnsondev/java-libpst
[libpst]: https://github.com/pst-format/libpst
[freepst]: https://github.com/hrbrmstr/freepst
[hrbrmstr/freepst]: https://github.com/hrbrmstr/freepst
[rJava]: https://github.com/s-u/rJava
