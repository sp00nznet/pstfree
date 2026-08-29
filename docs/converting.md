# OST to PST

`pstfree file.ost --rebuild out.pst`, or **File → Repair to a new .pst** in the window.

This is the third of the three things the payware sells, after "PST Repair" and "PST
Password Recovery", and it is the only one of them that involves real work. The password
is a CRC32. Repair is genuinely hard. Conversion is somewhere in between: entirely
ordinary, entirely documented, and a few hundred lines.

## Why it is not a copy

A repair copies blocks byte for byte and rebuilds only the index around them. That is why
it cannot corrupt a property — it never looks at one. A conversion cannot work that way,
because the two formats disagree about what a block is:

| | Outlook 2013+ OST | PST |
|---|---|---|
| page | 4096 bytes | 512 bytes |
| block data | up to about 64KB | **8176 bytes** |
| block contents | often zlib-compressed | never |
| page trailer | 24 bytes, 16-bit entry counts | 16 bytes, 8-bit counts |

So every block has to be decoded and inflated, and some of them are too big to be written
back out as they came. Nothing survives the trip unchanged except the bytes themselves.

## What it actually does

It rebuilds the file one level up from the blocks. For every node it takes the **data
stream** behind it — whatever chain of blocks and XBLOCKs the OST used — and lays that
stream out again the way a PST stores it: plain blocks, an XBLOCK over them when there is
more than one, an XXBLOCK over those when there are more than 1,021. Subnode trees are
rebuilt the same way and recursively, because an attachment is a subnode and an embedded
message is a subnode tree hanging off one.

Block ids are all new. Node ids, parents and local subnode ids are not: those are what the
mail is addressed by.

The output declares `NDB_CRYPT_NONE` and its blocks go out plain. Re-encoding them would
be work in exchange for nothing — the encoding is keyless and identical in every PST ever
written, which is the argument this whole project is built on.

## The one thing that makes it hard

**Block boundaries have to be preserved**, and that is not tidiness.

A heap-on-node is how every property context and every table is stored, and it addresses
its own allocations by *which block* they are in — the `hidBlockIndex` field of an HID,
MS-PST 2.3.1.1. Concatenating a stream and re-splitting it into neat 8176-byte pieces
would leave every one of those pointing somewhere else. The file would still open. It
would still parse. It would give back the wrong answers, which is worse than failing.

So a block is divided only when it is too big for a PST to hold, and when that happens and
the block is a heap, it is **reported rather than done quietly**. In the test OST no live
block is over 8176 bytes at all — the biggest heap is 5,226 — so the case is rare enough
that a message body or an attachment is what usually hits it, and those are addressed by
offset across the whole stream, where re-splitting is invisible.

## The check

Reading back what you wrote proves nothing, so the reference implementation reads both
sides. libpff opens the source OST and the converted PST and every folder, every message
and every property on every message is compared, by id and by value:

```
OST to PST, judged by libpff on both sides

  source OST        |  46 folders, 3 message(s), 202 properties
  converted to PST  |  46 folders, 3 message(s), 202 properties

  Identical, id for id and byte for byte.
```

That runs in `tests\bakeoff.py`. A stricter version runs in `cargo test`, without libpff:
every node's data stream is compared **block for block** against the source, and every
subnode tree recursively — the same bytes in the same blocks in the same order, which is
the invariant the heap depends on and which a bytes-only comparison would sail straight
past.

A second test drives the two levels no fixture reaches: a stream long enough to need an
XXBLOCK, which is what a 20MB attachment needs and nothing smaller does, and a node with
more subnodes than one SLBLOCK holds.

## What is not claimed

- **No converted file has been opened by Outlook.** There is no Outlook on the machine
  this was written on. libpff opens them and reads the same mail out; that is evidence,
  and it is not that claim.
- **An OST's sync-state nodes come across.** NID types `0x14`–`0x19` are the OST's record
  of its conversation with a server and are meaningless in a PST — 83 of the 308 nodes in
  the test file. They are kept, because throwing away what you do not understand is the
  wrong instinct in a recovery tool, and nothing so far suggests they do harm.
- **A pre-2013 OST is a different and much smaller case.** It is already laid out exactly
  as a PST is, and only its header says otherwise, so `--rebuild` copies the blocks as
  usual and rewrites the two header fields that name the file type. No sample of one has
  turned up to test it against.
