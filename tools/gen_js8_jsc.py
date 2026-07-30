#!/usr/bin/env python3
"""Generate the JSC dictionary blob from JS8Call's `jsc_map.cpp`.

JS8 compresses free text against a shared 262k-word dictionary. Upstream ships
it as two 6 MB C++ source files — one ordered by index, one alphabetically —
which is 12 MB of tables no human ever reads and no compiler enjoys. We keep the
same data as one deflate-compressed blob loaded with `include_bytes!`, about
1 MB on disk, inflated once on first use.

    python3 tools/gen_js8_jsc.py ~/Development/js8call

Only the index-ordered table is stored. The alphabetical order needed for
word-to-index lookup is rebuilt at load time from the same data, so the two can
never disagree — which is a real risk when both are shipped as separate tables.

Blob layout, all little-endian:

    magic  "JS8D"          4 bytes
    count  u32             number of entries
    lens   count x u8      length of each word, in index order
    words  concatenated    the words themselves, in index order
"""

import pathlib
import re
import struct
import sys
import zlib

MAGIC = b"JS8D"


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} <path-to-js8call-checkout>")
    src = pathlib.Path(sys.argv[1]).expanduser()

    text = (src / "jsc_map.cpp").read_text(encoding="latin-1")
    # Entries for the extended Latin-1 characters are written as escaped bytes
    # with an explanatory comment between the string and the numbers
    # (`{"\xa1" /* ... */, 1, 10704}`), so the comment has to be allowed for or
    # 32 entries silently vanish and every index past them shifts.
    rows = re.findall(
        r'\{"((?:[^"\\]|\\.)*)"\s*(?:/\*.*?\*/)?\s*,\s*(\d+),\s*(\d+)\}', text
    )
    if not rows:
        raise SystemExit("no dictionary entries found — has jsc_map.cpp changed shape?")

    words = []
    for i, (word, length, index) in enumerate(rows):
        if int(index) != i:
            raise SystemExit(
                f"entry {i} claims index {index}; the table is not in index order "
                "and the blob format assumes it is"
            )
        word = word.encode().decode("unicode_escape")
        if not 1 <= len(word) <= 255:
            raise SystemExit(f"entry {i} has an unrepresentable length: {len(word)}")
        words.append(word.encode("latin-1"))

    if len(set(words)) != len(words):
        # Duplicates would make word-to-index lookup ambiguous; upstream's own
        # lookup takes the first, so mirror that but say so.
        dupes = len(words) - len(set(words))
        print(f"note: {dupes} duplicate words; lookup will take the lowest index",
              file=sys.stderr)

    blob = MAGIC + struct.pack("<I", len(words))
    blob += bytes(len(w) for w in words)
    blob += b"".join(words)
    compressed = zlib.compress(blob, 9)

    out = pathlib.Path(__file__).resolve().parent.parent / (
        "crates/sdroxide-digi/src/js8/jsc_dict.bin"
    )
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_bytes(compressed)
    print(
        f"wrote {out}: {len(words)} words, "
        f"{len(blob) / 1e6:.2f} MB raw -> {len(compressed) / 1e6:.2f} MB deflated"
    )


if __name__ == "__main__":
    main()
