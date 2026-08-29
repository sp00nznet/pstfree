# Repair: when the index is gone

The part the money is charged for, and the part no other free tool does.

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

## It does not stop at the first bad byte

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

## Writing the file back out

```
pstfree broken.pst --rebuild fixed.pst --salvage
```

Reading a damaged store gets the mail out. `--rebuild` gets the *file* back, which is the
thing every paid tool actually charges for and the only outcome that ends with someone
double-clicking a `.pst` again.

Almost nothing has to be understood to do it, and that is the whole design. A damaged PST
is nearly always a broken **index** over intact **contents** — the blocks holding the
heaps, the property contexts and the tables are fine, and it is the B-trees, the header
and the checksums over them that are wrong. So every surviving block is copied byte for
byte, obfuscation and all, and only the index around them is built fresh. Nothing in the
repair path parses a property or a heap, which is exactly why it cannot corrupt one.

Precisely one byte of a copied block changes: a block trailer carries `wSig`, computed
from the block's own offset and id, and the offset is what a rebuild moves. The length,
the CRC over the data and the id all describe bytes that did not change, so they cross
over untouched. The allocation maps go out marked `INVALID_AMAP`, which is not a fudge but
the state MS-PST 2.6.1.3.7 defines for "rebuild these before writing" — Outlook does that
on open. Their *slots* are still reserved, because a rebuild writes them at fixed offsets
and anything living there would be overwritten.

The proof is not that pstfree can read what pstfree wrote, which proves nothing at all:

| damaged input | libpff on it | rebuilt, then libpff |
|---|---|---|
| B-tree roots zeroed | refuses to open | **reads all 4 messages** |
| 20 junk sectors | refuses to open | still refuses — *and pstfree says so first* |

That second row is the more important one. Those junk sectors landed on the block holding
the message store, and no index can point at bytes that are gone, so the rebuilt file
genuinely will not open. It says so, by name, instead of handing back a file that fails
silently later:

```
This file will NOT open: it has no message store (node 0x21).
```

One thing it refuses outright rather than half-doing. An **OST** is 4K pages and
zlib-compressed blocks, so turning one into a PST is a format conversion and not a repair.
It says so and points at `--export`.

## Any size

There used to be a second refusal, at 32MB, and it was the more painful one: the size at
which a PST stops being a test file and starts being somebody's mailbox.

Two things put it there. The first was that the whole rebuilt file was assembled in memory
before being written, which is fine for 32MB and absurd for 40GB. That is now a single
forward pass — the header, then the blocks in the order they were placed, then the two
trees — re-reading each block off the source as it goes and zero-filling the gaps. Nothing
larger than one block is ever held.

The second was the allocation maps. A PST reserves fixed slots through the file for four
kinds of map page, and a rebuild must not put a block where one goes, or Outlook writes the
map straight over it. MS-PST states where **AMap** and **PMap** pages land, in prose. For
**FMap** and **FPMap** it gives the coverage of a page — about 125MB and about 8GB — and of
the header's built-in copies — 32MB and 2GB — and then draws the rest in a figure. Those
numbers pin down the first of each and leave the recurrence to be inferred from a picture,
which is exactly the kind of guess that ends with a map page landing on somebody's mail.

So it is not inferred. Figure 3 shows all four appearing in a fixed order, always at the
head of an AMap section and nowhere else, so **all four slots are kept clear at the head of
every section** — 2KB in every 248KB, or 0.8% of the file. That is a superset of every
reading of the figure. Being wrong about an interval now wastes half a kilobyte instead of
destroying a block, and the ceiling has nothing left to hold it up.

The sweeps had to keep up too. Carving tests every 64-byte boundary in the file, and one
read call each is 600 million of them on a 40GB file; both sweeps now read a megabyte at a
time and work out of the buffer. On a 400MB file that took a sweep from 14 seconds to 0.4.

Checked rather than asserted: a 40MB file of blocks that no index mentions, carved and
rebuilt into a **41MB PST across 171 AMap sections** — every map slot in all 171 still
empty, every block re-read and matched against its own checksum, and the result opened by
libpff, which reads the mail back out of it.
