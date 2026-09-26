# Findings, and what is still open

What reading the specification against real files actually turned up — including the
places the first reading was wrong.

## Open questions

Honest list of what hasn't been checked yet. These get answered before any of them get
promised.

- **Attachment extraction has never seen a real attachment.** Not one of the three
  fixtures has any, so the code that pulls them out of the subnode tree is written and the
  MIME assembly around it is unit-tested, but the extraction itself has never run against
  real data. Treat it as unproven until a file with attachments turns up.
- ~~**The window shows plain text only.**~~ **Settled, by deciding not to render.** A
  message with only an HTML body is now read in the window like any other: the markup is
  taken off and the text kept, with the line breaks the tags describe. Rendering it
  properly would mean hosting a browser control — a whole other runtime, in a program
  whose promise is that there is nothing to install — or writing a layout engine, which is
  a larger project than reading PST files. What is knowingly lost is named in
  `src/html.rs`: a table becomes its cells one after another, an image leaves only its
  `alt` text, and a link keeps its text and loses its URL. The `.eml` export still carries
  the markup intact, which is the answer for anyone who wants the message as it was sent.

  One decision worth recording, because the obvious rule is wrong: a block-level tag ends
  a line only when there is something on it. Treating every `<p>`, `<div>` and `</div>`
  as a line break turns real mail — which nests block tags five and six deep — into a page
  of blank lines. An explicit `<br>` always breaks, because somebody typed it.

- **ANSI PST support has never seen a file Outlook wrote.** Outlook 97–2002 files read,
  export and convert, and the format differences are all covered: the 512-byte header with
  its 40-byte ROOT, 32-bit ids and offsets, 12-byte trailers with the id **before** the
  checksum rather than after it, a page footer at 496, a subnode block with no padding
  after the count, and an XBLOCK id array half the usual width. But **no public ANSI PST
  exists** — the same search that found no corpus of damaged files found no ANSI ones
  either — so the fixture is one `tests/ansi.rs` writes itself from MS-PST 2.2.2.6 and
  2.2.2.7.

  That is better evidence than it sounds, because the file is written from the
  specification's field order and read back by code that shares no constants with it, and
  because every one of those differences fails loudly rather than quietly if it is wrong:
  the trailer order shows up as every block failing its checksum, the entry widths as
  garbage node ids, the subnode header as a tree read one field to the left. It is still
  not the same claim as "a 1998 archive opens". If you have one, it is the single most
  useful thing anybody could send this project.

- ~~**`NDB_CRYPT_CYCLIC` is implemented but has never decoded a real file.**~~ Still true
  of a real file, but it now decodes a whole synthetic one — `tests/ansi.rs` builds an
  ANSI PST with every data block cyclically encoded and reads the contents back out. The
  cipher is symmetric, so that is this project's encoder checked against its own decoder
  and no more; what it does prove independently is that `bCryptMethod` is read from offset
  461 in an ANSI header rather than 513, and that only data blocks are decoded — both
  things that would otherwise fail silently.
- **`.msg` output has never been opened by Outlook**, because there is no Outlook on the
  machine it was written on. The compound file underneath is verified by a reader written
  against the format and independently by 7-Zip, and the property streams follow MS-OXMSG
  — but "a valid compound file with the right streams in it" is not the same claim as
  "Outlook opens it", and only the first has been tested.
- ~~**`PidTagBody` (`0x1000`) is absent from all three fixtures.**~~ **Settled, and the
  original reading was wrong.** `0x1000` is not absent; it is present on every message
  that actually has a plain-text body, and holds exactly the right text. The earlier
  conclusion came from messages that have no plain-text body at all — a distribution
  list, a contact and a free/busy record, none of which are mail. Nor is `0x1013` "the
  HTML body, there and correct": on the OST it holds 114 bytes of plain text with not one
  `<` in it. And `0x6619` is not where the plain text lives — it appears only alongside
  a body that is already in `0x1000` or `0x1013`, carrying the same text again as UTF-16.
  libpff reads all of this identically, so it is what Outlook wrote rather than a
  misparse. Export was already using `0x1000` and `0x1013`, so the only thing to fix was
  the typing (below).
- ~~**Named properties (`0x8000` and up) are shown by number, not by name.**~~ **Done.**
  Those ids are not fixed by any specification — each file numbers them as it happens to
  meet them, so the same number means different things in two PSTs and printing it alone
  is close to printing nothing. `--props` now reads the file's own map in node `0x61` and
  labels them:

  ```
  0x8004  time         2016-08-02 15:00  PSETID_Appointment 0x820D
  0x8005  time         2016-08-02 15:30  PSETID_Appointment 0x820E
  0x800E  string       "someone@example.com"  PSETID_Common 0x8580
  ```

  Which is checkable rather than merely plausible: `0x820D` is `PidLidAppointmentStartWhole`
  and `0x820E` the matching End, and the two times are half an hour apart on the
  appointment that says it is half an hour long. A map off by a single entry would still
  print something that looked fine, and would not survive that. Nine well-known property
  sets are spelled out by name and the rest print as GUIDs. A file that has lost node
  `0x61` still lists its properties, and says why they have no labels.
- **NID types `0x14`–`0x19` are not in MS-PST**, which lists them as unallocated. The test
  OST is full of them — 40 of type `0x14` and 39 of `0x15` in a file with 40 folders, so
  roughly one of each per folder. Best guess is the sync engine's per-folder state, which
  would be OST-only. Currently labelled as undocumented rather than guessed at.
- **Where FMap and FPMap pages actually recur is still not known**, and a rebuild now
  works around that rather than answering it — see below. Settling it needs a PST over
  125MB written by Outlook, which is the one thing no public corpus has.
- **No rebuild of any size has been opened by Outlook**, large ones included. There is no
  Outlook on the machine this was written on. libpff opens them and every block in them
  re-reads and re-checksums, which is evidence, but it is not that claim.
- **Encrypted OST.** Per MS-PST the encoding modes are keyless, but Microsoft 365 profiles
  can restrict a local cache in ways this repo hasn't tested. Needs a real sample.
- **A node whose every surviving index entry is stale still recovers as an older
  revision** — nothing can be done about that, the newer entry is genuinely not in the
  file any more. It is named now rather than passed off silently: a sweep that had to
  choose between disagreeing copies of a node's entry lists the nodes it chose for, and
  says which ones point at data that is no longer in the file at all. A node id can be
  taken to `--props` and the message read; "some of your mail may be an old copy" is not
  something anyone can act on. The warning is printed only when the sweep *is* the index.
  While the file's own index is readable it settles all of this, and warning then would
  be crying wolf over a healthy file.

## The Enron corpus, and what it can and cannot test

The CMU Enron release (`enron_mail_20150507.tar.gz`, 1.7GB unpacked) was the obvious big
dataset to throw at this, and it is worth writing down exactly what it is before it gets
mistaken for a PST corpus. Every one of its 517,401 messages was surveyed straight out of
the tarball:

| | |
|---|---|
| Container | Maildir: one RFC 5322 text file per message. **No PST, no OST.** |
| Where it came from | `X-FileName`: 352,254 from Lotus Notes `.nsf`, 161,001 from `.pst`, 4,143 unknown |
| Attachments | None — stripped, per the release notes |
| Charset declared | `us-ascii` on every single message; 90 files hold any byte over 0x7F |
| `Message-ID` | Every one minted by the conversion (`…JavaMail.evans@thyme`), not the original |
| `Received`, transport headers, signatures | None, on any message |

So it unblocks **none** of milestones 17–20: there is no PST in it to read, no attachment,
no ANSI file and nothing Outlook wrote. It cannot exercise the code page work either,
because the conversion flattened everything to ASCII. What it *is* good for is volume —
half a million real messages with real subjects, senders and bodies — which is exactly
what the three fixtures lack, and what a search, an export or a rebuild at mailbox scale
needs. Using it that way means building PSTs out of it, which is a separate piece of work
(see the roadmap), and the result would be a PST this project wrote, not one Outlook did.

### Could this tool settle whether the corpus is authentic?

Two papers by Kenji Nakamura ("Serious Doubt on the Authenticity of the Enron Email
Corpus", 2024, and a 2026 follow-up) argue that because a July 2000 thread discusses
impersonation, any message in the corpus might be forged. The thread is in the tarball at
`maildir/cash-m/all_documents/792.` and reads plainly. Two separate things are in it:

1. An anonymous complaint about the review process was sent from a **shared role
   mailbox** — "Office of the Chairman@ECT" — and legal wanted to know who had access to
   it. That is somebody with access to a group mailbox, not a forged header.
2. Michelle Cash had heard that someone could "hack into the emeet site and pretend to be
   Jeff Skilling". eMeet was an intranet discussion board (the corpus links it as
   `eThink/eMeet.nsf`, a Notes database), not the mail system.

Neither is evidence that the corpus misrepresents the mailboxes it was collected from.
That a sender line could be falsified in 2000 was true of all email everywhere, and a
corpus recording its own organisation worrying about it is, if anything, a sign it was not
sanitised. The papers run two different questions together: *could a given message have
been sent by someone other than its named sender* (yes, for any mail of that era) and *is
the collection a faithful copy of what was seized* (a chain-of-custody question, which no
header can answer).

**Can pstfree prove it either way? No, and not for want of a feature.** The CMU release
has had everything that could bear on it removed: no transport headers, no `Received`
chain, the original Message-IDs replaced, times rewritten into the converter's time zone.
Nor was there anything cryptographic to lose — DKIM did not exist until 2007, and internal
Notes or Exchange mail never carried a signature. There is nothing left in the file for
any tool to check.

What *would* be in reach, given an original custodian PST: MAPI records more about how a
message was sent than any maildir does. `PidTagSenderName` beside
`PidTagSentRepresentingName` is exactly the "sent on behalf of a shared mailbox" case
above; `PidTagTransportMessageHeaders` holds the `Received` chain for anything that came
from outside; `PidTagCreatorName` and `PidTagLastModifierName` say who made the item and
who last changed it. Showing those side by side is a provenance view, and a reasonable
feature — it shows the evidence the file holds rather than delivering a verdict. For
modern mail, checking a DKIM signature on an exported message is possible in principle
with nothing but Windows (`BCrypt` for RSA, `DnsQuery` for the key), but keys get rotated
and retired, so an old genuine message routinely fails. A tool that stamped "not
authentic" on it would be wrong in the direction that hurts people. That part is a bridge
too far.

## Resolved along the way

- **Narrow text is decoded in the code page the message declares.** Every `PtypString8`
  property — all of them, in an ANSI PST — and every HTML body used to be read as UTF-8
  or else windows-1252, so a Japanese or Chinese message came out as mojibake in the
  window and in search while its exported `.eml` was fine. The file does say which code
  page: `PidTagMessageCodepage` (`0x3FFD`) on the message for its narrow strings,
  `PidTagInternetCodepage` (`0x3FDE`) for the HTML body. Windows already knows every code
  page there is, so `MultiByteToWideChar` decodes them and no character-set library came
  in. UTF-8 is still tried first, because declarations are wrong in that direction often
  (an HTML body whose `<meta>` says UTF-8 beside a property that says 1252) and Shift-JIS
  or GBK bytes are almost never valid UTF-8 by accident.

  What did not work out as expected: `MB_ERR_INVALID_CHARS` rejects far less than its
  name suggests. Windows maps even the bytes Shift-JIS leaves undefined to *something*,
  and a lead byte with nothing after it becomes 「・」. So a declared code page is trusted
  rather than cross-checked, and the fallback to windows-1252 is only for a code page the
  machine does not have. Table rows — the recipient list is the one that matters — carry
  no code page column and still get UTF-8-or-1252.

- **Search runs off the message loop.** The window used to search on the message loop
  itself, behind a wait cursor, which was instant on the fixtures and would have frozen
  the window for minutes on a real mailbox. It is a job now, like export and repair: it reopens the
  file on a worker, reports progress in the status bar, and hands its hits back to the
  list when it is done.

- **Converting an OST does not mean patching block references, it means not keeping any.**
  The first plan was to copy blocks across and re-split only the ones too big for a PST,
  then patch every reference to the ones that moved. That is a trap: an XBLOCK's children
  must be data blocks, so a data block that becomes an XBLOCK cannot be swapped in where
  it used to sit, and the same id cannot carry over anyway because a BID's `fInternal` bit
  is what tells a reader whether to decode the block. Working one level up — taking each
  node's whole *data stream* and laying it out again — removes the problem rather than
  solving it. Nothing points at a block afterwards except the index this code writes.

- **The one thing that constrains it is that heaps address themselves by block number.**
  An HID names an allocation as (which block, which allocation) — MS-PST 2.3.1.1 — so a
  stream concatenated and re-split into neat 8176-byte pieces would leave every property
  context and every table pointing at the wrong place. It would open, parse, and answer
  wrongly, which is worse than failing. So the original block boundaries are kept and a
  block is divided only when it is too big for a PST to hold; when that happens and the
  block is a heap, it is reported rather than done quietly. The test asserts the stream
  comes back **block for block**, not merely byte for byte, because a bytes-only
  comparison would sail straight past exactly this.

- **The oversized blocks in the OST fixture were freed ones, not live ones.** An early
  measurement said two of 442 blocks inflate to 46,397 bytes, and that was a carve of the
  whole file including space that had been released. Of the 310 blocks the live index
  names, none is over 8176 and the biggest heap is 5,226. The format allows far bigger and
  a real mailbox will have them; this fixture does not, so the splitting path is checked
  by construction rather than by the fixture.

- **The 32MB rebuild ceiling was a specification gap, and it did not need closing.** A PST
  reserves fixed slots for four kinds of allocation map page, and a rebuild that put a
  block in one would have Outlook write the map over it. MS-PST states where AMap and PMap
  pages go. For FMap and FPMap it gives the coverage of a page (about 125MB, about 8GB) and
  of the header's own copies (32MB, 2GB) — which pin the first of each and leave the
  recurrence to a figure. Reading a recurrence off a picture is how a repair tool destroys
  a block, so the ceiling stood.

  It turned out the interval never had to be known. The figure shows all four maps in a
  fixed order, at the head of an AMap section and nowhere else, so all four slots are now
  kept clear at the head of *every* section. That is a superset of any reading of the
  figure and costs 2KB in every 248KB — 0.8%. Being wrong about an interval is now merely
  wasteful. The other half was that the whole file was assembled in a `Vec` before being
  written, which a 40GB mailbox does not fit in; it is a single forward pass now, holding
  one block at a time. Checked at 41MB across 171 AMap sections: every slot still empty,
  every block re-read against its own checksum, and libpff reads the mail back out.

- **Both sweeps were doing one read call per step, and one of the steps is 64 bytes.**
  Carving tests every aligned boundary in the file for a block trailer — 600 million of
  them on a 40GB file, each formerly a seek and a read. They read a megabyte at a time now
  and work out of the buffer, which also removes the second read carving used to make in order to
  checksum a candidate. On a 400MB file: 14 seconds to 0.4. The other half of that win was
  a bound MS-PST supplies and the code was not using — a block is at most 8KB — so a
  trailer made of random bytes can no longer ask for a 64KB checksum before being rejected.

- **The sweep can be checked against ground truth, and it was wrong.** An undamaged file
  carries both the authoritative index *and* the freed pages the sweep reads, so the sweep
  can be run where the right answer is already known. Doing that found a real fault: with
  the tie between disagreeing copies broken on the data block alone, one node in
  `dist-list.pst` came back carrying an older subnode tree — a message whose attachments
  had moved but whose body had not. Breaking the tie on `bid_sub` as well fixed it, and
  all three fixtures now reproduce their own index exactly, node for node. It is a test
  now, and it is the only real evidence recovery has.
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
- **The folder tree comes from the node B-tree's parent pointers; the folders' own tables
  are the cross-check.** Both are read now, and `--verify` compares them — two independent
  records of the same fact, which agree exactly in an undamaged file and are worth having
  precisely for when they do not. In the test files, 36 tables agree with the pointers
  entry for entry.
- **`.msg` needed a compound file writer, which is 300 lines and no dependency.** Sectors,
  a FAT, a directory tree ordered by the format's own comparison (name length first, then
  uppercased name), and a mini-stream so a 40-byte property table does not cost a whole
  512-byte sector.
