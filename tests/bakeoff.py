"""Head-to-head: pstfree vs libpff (pypff) on the same damaged files.

Needs the reference implementation: pip install libpff-python. Not a cargo test,
because a Python dependency has no business gating `cargo test`.

libpff is the reference implementation everyone else wraps. If it reads a file and
pstfree does not, pstfree is wrong. If pstfree recovers mail from a file libpff
refuses, that is the entire pitch of the project, measured instead of asserted.
"""
import os, re, struct, subprocess, sys, tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(tempfile.gettempdir(), "pstfree-bakeoff")
PSTFREE = os.path.join(REPO, "target", "release", "pstfree.exe")
OFF_ROOT = 180
OFF_BREF_NBT = OFF_ROOT + 36   # bid(8) then ib(8)
OFF_BREF_BBT = OFF_ROOT + 52


def page_size(b):
    ver = struct.unpack_from("<H", b, 10)[0]
    return 4096 if ver in (36, 37) else 512


def variants(name, b):
    """The damage, built the same way for both tools."""
    yield f"{name} [intact]", b

    # Both B-tree roots wiped: the index is gone, only a sweep can get the mail out.
    z = bytearray(b)
    ps = page_size(b)
    for off in (OFF_BREF_NBT, OFF_BREF_BBT):
        at = struct.unpack_from("<Q", b, off + 8)[0]
        z[at:at + ps] = b"\0" * ps
    yield f"{name} [B-tree roots zeroed]", bytes(z)

    # The back 40% of the file simply not there, as when a copy dies partway.
    yield f"{name} [truncated to 60%]", b[: len(b) * 6 // 10]

    # The fuzzer's own damage: sector-aligned junk, seeded so this is reproducible.
    s = 0x2545F4914F6CDD1D
    r = bytearray(b)
    for _ in range(20):
        s = (s * 6364136223846793005 + 1442695040888963407) & (2**64 - 1)
        at = (s % len(r)) // 512 * 512
        r[at:at + 512] = bytes((s >> (i % 8 * 8)) & 0xFF for i in range(512))
    yield f"{name} [20 junk sectors]", bytes(r)


def ask_libpff(path):
    import pypff
    f = pypff.file()
    try:
        f.open(path)
    except Exception as e:
        return "refused to open", 0
    try:
        n = [0]

        def walk(folder, depth=0):
            if depth > 32:
                return
            for i in range(folder.get_number_of_sub_messages()):
                folder.get_sub_message(i)
                n[0] += 1
            for i in range(folder.get_number_of_sub_folders()):
                walk(folder.get_sub_folder(i), depth + 1)

        walk(f.get_root_folder())
        return "read", n[0]
    except Exception as e:
        return f"failed: {type(e).__name__}", 0
    finally:
        try:
            f.close()
        except Exception:
            pass


def ask_pstfree(path):
    for args in ([], ["--salvage"]):
        try:
            p = subprocess.run([PSTFREE, path, "--list"] + args,
                               capture_output=True, text=True, timeout=180)
        except subprocess.TimeoutExpired:
            return "timed out", 0
        # Trust pstfree's own footer, not a guess at its table: a message with no
        # delivery time prints a blank date, and counting dated lines silently loses it.
        m = re.search(r"^(\d+) message\(s\)", p.stdout, re.M)
        n = int(m.group(1)) if m else 0
        if n:
            return ("read" if not args else "read (salvage)"), n
        if p.returncode != 0:
            return f"exit {p.returncode}", 0
    return "no messages", 0


def rebuilds():
    """Damaged in, repaired out, and libpff as the judge of whether it worked.

    This is the only test of a repair that means anything: pstfree reading a file pstfree
    wrote proves nothing at all. A file the reference implementation refuses, rebuilt into
    one the reference implementation reads, is the whole claim in one line.
    """
    print("\nrepair, judged by libpff\n")
    print(f"  {'damaged input':34} | {'libpff on it':16} | libpff on the rebuild")
    print("  " + "-" * 78)
    for name in ("dist-list.pst", "passworded.pst"):
        src = os.path.join(REPO, "tests", "data", name)
        if not os.path.exists(src):
            continue
        for label, data in variants(name, open(src, "rb").read()):
            dmg = os.path.join(OUT, "r_" + re.sub(r"[^\w.-]", "_", label))
            open(dmg, "wb").write(data)
            fixed = dmg + "-fixed.pst"
            p = subprocess.run([PSTFREE, dmg, "--rebuild", fixed, "--salvage"],
                               capture_output=True, text=True, timeout=300)
            after = ask_libpff(fixed)[0] if os.path.exists(fixed) else "not written"
            if "NOT open" in p.stdout:
                after += " (pstfree said so)"
            print(f"  {label:34} | {ask_libpff(dmg)[0]:16} | {after}")


def compare_properties():
    """Every property of every message, both tools, and where they disagree.

    Counting messages only proves the folders were walked. This goes a level down and
    asks whether the two read the same properties off the same node, which is where a
    silent misparse would actually show up.
    """
    import pypff

    print("\nproperties, per message\n")
    for name in ("dist-list.pst", "example-2013.ost", "passworded.pst"):
        path = os.path.join(REPO, "tests", "data", name)
        if not os.path.exists(path):
            continue
        f = pypff.file()
        f.open(path)

        def walk(folder, out):
            for i in range(folder.get_number_of_sub_messages()):
                out.append(folder.get_sub_message(i))
            for i in range(folder.get_number_of_sub_folders()):
                walk(folder.get_sub_folder(i), out)
            return out

        checked, props, differed = 0, 0, []
        for m in walk(f.get_root_folder(), []):
            nid = m.get_identifier()
            theirs = set()
            for i in range(m.number_of_record_sets):
                rs = m.get_record_set(i)
                for j in range(rs.number_of_entries):
                    theirs.add(rs.get_entry(j).entry_type)

            p = subprocess.run([PSTFREE, path, "--props", f"{nid:X}"],
                               capture_output=True, text=True, timeout=120)
            ours = {int(x, 16) for x in re.findall(r"^\s+0x([0-9A-F]{4})\s", p.stdout, re.M)}

            checked += 1
            props += len(theirs)
            if theirs != ours:
                differed.append((nid, sorted(theirs - ours), sorted(ours - theirs)))
        f.close()

        print(f"  {name}: {checked} message(s), {props} properties, "
              f"{len(differed)} disagreeing")
        for nid, missing, extra in differed:
            print(f"    0x{nid:X}  libpff only: {[hex(x) for x in missing]}"
                  f"  pstfree only: {[hex(x) for x in extra]}")


def main():
    os.makedirs(OUT, exist_ok=True)
    rows = []
    for name in ("dist-list.pst", "example-2013.ost", "passworded.pst"):
        src = os.path.join(REPO, "tests", "data", name)
        if not os.path.exists(src):
            continue
        b = open(src, "rb").read()
        for label, data in variants(name, b):
            path = os.path.join(OUT, re.sub(r"[^\w.-]", "_", label))
            open(path, "wb").write(data)
            rows.append((label,) + ask_libpff(path) + ask_pstfree(path))

    w = max(len(r[0]) for r in rows)
    print(f"{'case'.ljust(w)} | {'libpff':<22} {'msgs':>5} | {'pstfree':<16} {'msgs':>5}")
    print("-" * (w + 60))
    for label, ls, ln, ps, pn in rows:
        print(f"{label.ljust(w)} | {ls:<22} {ln:>5} | {ps:<16} {pn:>5}")

    rebuilds()
    compare_properties()


if __name__ == "__main__":
    main()
