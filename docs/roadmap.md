# Roadmap

Updated as things land. Nothing is claimed until it runs.

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
| 3c | Table contexts — recipients, and a second opinion on membership | ✅ done |
| 4c | Export to `.mbox` and `.msg` | ✅ done |
| 8 | The window | ✅ done |
| 9 | `--rebuild` — write the damage back out as a clean `.pst` | ✅ done — Unicode PST, under 32MB |
| 10 | Rebuild a mailbox-sized PST: no size ceiling, and a sweep that finishes | ✅ done — verified at 726MB |
| 11 | Repair from the window, a readable damage report, and progress on the long jobs | ✅ done |
| 12 | OST → PST: decode, inflate and lay every data stream out again | ✅ done — libpff agrees on both sides |
| 13 | Rich-text messages readable in the window, without a browser in the process | ✅ done |
| 14 | Search the whole mailbox — subject, sender and body | ✅ done — `--find`, and the box in the window |
| 15 | Export one folder or one message instead of all of it | ✅ done |
| 16 | ANSI PST (Outlook 97–2002): read it, and convert it to Unicode | ✅ done — against a fixture this repo writes |
| — | The window: an icon, a toolbar, the system font, and a scaled display | ✅ done — no build script, no dependency |
| 21 | Search in the window without freezing it | ✅ done — a job, with progress, like export |
| 22 | Every code page Windows knows, as the message declares it | ✅ done — `MultiByteToWideChar`, no charset library |

66 tests, verified against a real PST, a real 2013 OST and a real password-protected PST —
the public fixtures from freepst, fetched by `tests\fetch-fixtures.ps1` — plus a synthetic
ANSI PST, since no public one exists. Test files are not committed, because real PSTs
contain real mail; the tests skip rather than fail when they are absent.

## What is left

Nothing on this list is promised. The first four are blocked on the same thing: a file
nobody has yet handed over. The CMU Enron corpus looked like it might be that file and is
not — it is maildir text, with no PST, no attachment and nothing Outlook wrote in it. See
[findings](findings.md#the-enron-corpus-and-what-it-can-and-cannot-test).

| | Next | What it needs |
|---|---|---|
| 17 | Attachments proven against a real one | Any PST with an attachment in it |
| 18 | ANSI proven against a file Outlook 97 wrote | One real ANSI `.pst` |
| 19 | Where FMap and FPMap pages recur, settled rather than worked around | A PST over 125MB written by Outlook |
| 20 | A rebuild opened by Outlook rather than by libpff | A machine with Outlook on it |
| 23 | Build a PST out of a maildir, to test search, export and rebuild at 500,000 messages | Nothing but the work: the Enron corpus is the input. The result is a PST this project wrote, so it tests scale and not compatibility |
| 24 | A provenance view: sender beside sent-on-behalf-of, the `Received` chain, creator and last modifier | Nothing but the work. Shows the evidence a message carries; does not claim to judge it |
