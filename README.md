# pstfree

Read, export and repair Outlook `.pst` and `.ost` files on Windows. No Outlook
required, no licence, no per-mailbox fee, no fake progress bar. MIT licensed.

**Status: early.** It opens PST and OST files, walks both B-trees and tells you what is
in them and what is wrong with them. It does not read your mail yet. See
[Progress](#progress).

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
  password** in the message store. The block obfuscation (`NDB_CRYPT_PERMUTE` /
  `NDB_CRYPT_CYCLIC`) uses a **fixed table with no key at all** — it is identical whether
  the file has a password or not. Any reader can simply not ask. The products charging
  $30–$50 to "recover" a PST password are charging you to skip an `if` statement.
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
target\release\pstfree.exe archive.pst
```

One executable, no dependencies, no runtime, no installer. It never asks for a password.

```
tests\data\passworded.pst
  PST, Unicode format (version 23, 512-byte pages)
  271360 bytes on disk, 271360 declared in the header
  block encoding: permute - a fixed substitution table, no key

  130 nodes, 138 blocks, 52912 bytes of block data
          9  0x01  internal
         18  0x02  folder
          3  0x04  message
         13  0x08  associated message
         19  0x0D  hierarchy table
         ...

  No structural damage found.
```

That file is password-protected. Nothing above asked for the password, because there is
nothing there to ask about.

`--nodes` and `--blocks` dump the two B-trees entry by entry.

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

**Export** — `.eml` and `.msg` per message, `.mbox` per folder, attachments to disk,
and a manifest. Rebuild a clean `.pst` from a damaged one.

**Repair** — the differentiator, and the only genuinely hard part:

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

- **NID types `0x14`–`0x19` are not in MS-PST**, which lists them as unallocated. The test
  OST is full of them — 40 of type `0x14` and 39 of `0x15` in a file with 40 folders, so
  roughly one of each per folder. Best guess is the sync engine's per-folder state, which
  would be OST-only. Currently labelled as undocumented rather than guessed at.
- **Encrypted OST.** Per MS-PST the encoding modes are keyless, but Microsoft 365 profiles
  can restrict a local cache in ways this repo hasn't tested. Needs a real sample.
- **How far past `scanpst.exe` can a rebuild actually get?** Truncation is the easy damage
  case and it already works. A torn B-tree is the real test and is not written yet.
- **The two crypt tables.** `NDB_CRYPT_PERMUTE` and `NDB_CRYPT_CYCLIC` need the fixed
  tables from MS-PST 5.1. Not needed to survey a file — pages and B-trees are never
  encoded — but nothing above the node layer can be read without them.
- **Page and block CRCs** need the table from MS-PST 5.3. Structure and block identity are
  checked already, which catches a torn index; the CRC is what tells "wrong page" apart
  from "right page, rotten bytes", so the repair pass will want it.

Resolved along the way:

- **Rust, no dependencies.** Single static exe, no runtime, matching vncfree. Parsed from
  the spec rather than wrapping libpff, which keeps the licence MIT.
- **ANSI PST is refused, not half-parsed.** Different header layout, 2GB ceiling, Outlook
  97–2002 only. It says so plainly instead of producing wrong answers.
- **OST 2013+ is a different page layout** and nothing said so up front. `wVer 36` uses
  4096-byte pages with the trailer 24 bytes from the end, not 16, and 16-bit entry counts.
  Established by reading a real file. Both variants are handled.

## Progress

Updated as things land. Nothing is claimed here until it runs.

| | Milestone | State |
|---|---|---|
| 0 | Repo, scope, prior-art review | ✅ done |
| 1 | Header, node and block B-trees, node survey | ✅ done — PST and OST, both page layouts |
| 4a | The password no-op | ✅ done — it was never asked for |
| 5a | Damage report — truncation, bad pages, loops, wrong ids | ✅ done |
| 2 | Blocks: the two crypt tables, then heap and property contexts | 🟡 next |
| 3 | The folder tree with real names, then messages and attachments | ⬜ not started |
| 4b | Export — eml / msg / mbox | ⬜ not started |
| 5b | Page and block CRCs, to separate wrong pages from rotten ones | ⬜ not started |
| 6 | Rebuild torn B-trees | ⬜ not started |
| 7 | Carve orphaned nodes | ⬜ not started |
| 8 | GUI | ⬜ not started |

Verified against a real PST, a real 2013 OST and a real password-protected PST — the
public fixtures from freepst, fetched by `tests\fetch-fixtures.ps1`. Test files are not
committed, because real PSTs contain real mail.

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
