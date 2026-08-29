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
- **The window shows plain text only.** A message with an HTML body and no plain-text one
  says so rather than rendering it; export to read those. Rendering HTML means either
  hosting a browser control or writing a layout engine, and neither belongs in a reader.
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
- **`NDB_CRYPT_CYCLIC` is implemented but has never decoded a real file.** No fixture uses
  it. The specification calls it a symmetric cipher and the test checks that running it
  twice returns the original bytes, which is the only evidence behind it. Permute is
  verified against real files and can be trusted; cyclic cannot be, yet.
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

## Resolved along the way

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
