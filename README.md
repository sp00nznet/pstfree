# pstfree

Read, export and repair Outlook `.pst` and `.ost` files on Windows. No Outlook
required, no licence, no per-mailbox fee, no fake progress bar. MIT licensed.

**Status: nothing is built yet.** This README is the whole project. See
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

- **Encrypted OST.** Per MS-PST the encoding modes are keyless, but Microsoft 365 profiles
  can restrict a local cache in ways this repo hasn't tested. Needs a real sample.
- **How far past `scanpst.exe` can a rebuild actually get?** The claim above is the whole
  premise and it is currently a hypothesis. Needs deliberately corrupted files and
  measured results.
- **Language and runtime.** Rust for a single static exe with no runtime, matching vncfree,
  is the obvious default — but a thin shell over libpff would be dishonest about the LGPL
  and slower to ship than it looks. Parse from the spec.
- **Test corpus.** Real PSTs contain real mail. Need generated ones, plus the public
  samples from libpff/java-libpst test suites.

## Progress

Updated as things land. Nothing is claimed here until it runs.

| | Milestone | State |
|---|---|---|
| 0 | Repo, scope, prior-art review | ✅ done |
| 1 | Parse the header + node/block B-trees; dump the folder tree | ⬜ not started |
| 2 | Read messages, properties, bodies, attachments | ⬜ not started |
| 3 | Export — eml / msg / mbox | ⬜ not started |
| 4 | OST, and the password no-op | ⬜ not started |
| 5 | Damage report — say what's wrong in English | ⬜ not started |
| 6 | Rebuild torn B-trees | ⬜ not started |
| 7 | Carve orphaned nodes | ⬜ not started |
| 8 | GUI | ⬜ not started |

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
