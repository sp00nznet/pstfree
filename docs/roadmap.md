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
| 10 | Rebuild a mailbox-sized PST: no size ceiling, and a sweep that finishes | ✅ done — verified at 41MB |

47 tests, verified against a real PST, a real 2013 OST and a real password-protected PST —
the public fixtures from freepst, fetched by `tests\fetch-fixtures.ps1`. Test files are
not committed, because real PSTs contain real mail; the tests skip rather than fail when
they are absent.
