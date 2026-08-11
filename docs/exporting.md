# Getting the mail out

`.eml`, `.mbox` and Outlook's own `.msg`, and the rules the exporter holds to.

## Getting the mail out

```
> pstfree.exe mailbox.ost --export .\mail
3 message(s) written to .\mail

.\mail\Root - Mailbox\IPM_SUBTREE\Inbox\Microsoft Outlook Test Message.eml
.\mail\Root - Mailbox\IPM_SUBTREE\Inbox\Microsoft Outlook Test Message (0x2000E4).eml
.\mail\Root - Mailbox\IPM_SUBTREE\Sent Items\Test 2.eml
```

One directory per folder, one `.eml` per message — RFC 5322 with MIME, which Thunderbird,
Outlook, `mutt` and everything else will open. Attachments are included as MIME parts.

`--format mbox` writes one file per folder instead, with messages concatenated the way
mail archives keep them. Lines that begin `From ` are escaped, and so are ones that already
begin `>From `, because otherwise unescaping later would eat a `>` the sender wrote.

`--format msg` writes Outlook's own format, which keeps the MAPI properties a mail message
has nowhere to put. That means writing a compound file — a filesystem inside a file, with
sectors, a FAT and a directory of named streams. It is verified two ways: a reader written
against the format rather than against the writer round-trips everything back out, and
7-Zip, which has never heard of this project, lists the result correctly:

```
> 7z l "Test 2.msg"
Extension = compound
   Size   Name
     12   __substg1.0_0037001F          <- subject
   3032   __substg1.0_007D001F          <- the original internet headers
   5816   __substg1.0_10130102          <- the HTML body
    224   __properties_version1.0
          __recip_version1.0_#00000000  <- a storage, with the recipient inside
     60   __recip_version1.0_#00000000\__substg1.0_39FE001F
```

**Recipients come from the message's recipient table**, so `To:` and `Cc:` carry the
addresses mail was actually sent to rather than the names Outlook happened to display.

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
