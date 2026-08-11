# How this is tested

Three independent checks: a reference implementation, a fuzzer, and the one piece of
ground truth a PST can supply about its own recovery.

## Measured against libpff

[libpff] is the reference implementation nearly every other PST tool wraps, and it is the
honest yardstick: where it reads a file and this does not, this is wrong. `tests/bakeoff.py`
builds the same damaged files for both and asks each one how much mail it can get out
(`pip install libpff-python` first — a Python dependency has no business gating
`cargo test`, so it is a script rather than a test).

| Case | libpff | pstfree |
|---|---|---|
| all three fixtures, intact | reads, 4 / 3 / 3 messages | **identical, 4 / 3 / 3** |
| every property of every message | 10 messages, 686 properties | **identical, id for id** |
| B-tree roots zeroed | refuses to open | reads all of them |
| truncated to 60% | reads | reads |
| 20 junk sectors | refuses to open, or `OSError` | reads all of them |
| **a `--rebuild` of the roots-zeroed file** | **reads it** | — |

Exact agreement on every undamaged file, and not only on the message count: every
property of every message, 686 of them, id for id, with nothing either tool sees that the
other misses. That is the part that matters most — a repair tool that quietly disagrees
with the reference on healthy input is not a repair tool. It also earned its keep
immediately, catching a body exported as `text/html` that had no markup in it. On
the six damaged files libpff opened none and this opened all six, which is the pitch, now
measured rather than asserted. On the OST with its roots zeroed the sweep returns **seven**
messages where the intact file has three: the extra four are deleted mail whose index
entries were dropped and whose freed pages still hold them.

## Randomly mangled files

Recovery has only ever been tested against damage this repo inflicted itself. There
is no public corpus of broken PSTs to test against — the EDRM Enron PST sets are all
dead links now, the Digital Corpora forensic scenarios turn out to contain no PST or OST
at all, and no PST library ships a fixture. So the damage is still self-inflicted, but
it is no longer only the tidy kind. `tests/fuzz.rs` mangles the real fixtures in the
five shapes real storage fails in, all sector-aligned because a disk does not fail at
byte granularity: a wiped run, a junk run, a single flipped bit, sectors *transplanted
from elsewhere in the same file*, and sectors of a *different file* entirely. Half the
damage is aimed at the first 16KB, where the fields a parser trusts live — a wrecked
mail body is a wrong subject line, a wrecked count is something to loop on. Each result
is read, swept and carved under a panic guard and a timeout.

The last two shapes are the interesting ones, and they are why this is a fuzzer and not
a disk image. Putting the file on a virtual disk and corrupting *that* mostly tests
NTFS; the one thing it produces that matters is a file with foreign clusters spliced
into it, and those bytes can be synthesized directly in five lines. A transplanted
sector is real structure with a real checksum sitting where it does not belong — which
is the shape of the stale-revision and ghost-block problem, and nothing a wipe can make.

It found one: `walk` checked that a page's declared entries *fit* in the page but never
that `cbEnt` was big enough for the entry it described, so a page claiming 8-byte
entries panicked every reader that indexed past offset 8. It is a diagnosis now, and
4000 mangled files since have produced no panic and no hang.
What none of this proves is that recovery returns the *right answer* on real-world
damage. No amount of random splatter will. Set `PSTFREE_FUZZ_ROUNDS` to hunt harder;
a failing round keeps its file on disk and its seed replays it exactly.

## The sweep, against ground truth

An undamaged file carries both the authoritative index *and* the freed pages the sweep
reads, so the sweep can be run where the right answer is already known — and where the two
disagree, the sweep is simply wrong. That is the only ground truth recovery has, and it
found a real fault: with the tie between disagreeing copies broken on the data block
alone, one node came back carrying an older subnode tree. All three fixtures now reproduce
their own index exactly, node for node, and it is a test.

## Running it

```
cargo test                      # 45 tests, no fixtures needed to build
testsetch-fixtures.ps1        # the three public fixtures, for the rest
pip install libpff-python       # the reference implementation
python testsakeoff.py         # the head-to-head above
set PSTFREE_FUZZ_ROUNDS=400 && cargo test --release --test fuzz
```

[libpff]: https://github.com/libyal/libpff
