#!/usr/bin/env python3
"""Generate `crates/sdroxide-digi/src/js8/ldpc_tables.rs` from JS8Call source.

The LDPC(174,87) tables are large enough that hand-transcription would be a
liability, so they are machine-extracted instead. Run this only when the
upstream commit in `vendor/js8call/PROVENANCE.md` changes; the output is
committed, so a normal build needs neither Python nor a JS8Call checkout.

    python3 tools/gen_js8_ldpc.py ~/Development/js8call

Before emitting anything it verifies the two tables against each other:

  * `Mn` (checks per bit) and `Nm` (bits per check) must describe the same
    522-edge bipartite graph.
  * The generator must produce codewords satisfying every parity check in `Nm`.

`Mn`/`Nm` and the generator are independent transcriptions of the same code, so
the second assertion is a genuine cross-check rather than a self-consistent one.
It also pins the row-order question: JS8Call's Fortran needs a `colorder`
permutation after encoding, while its C++ port folds that permutation into the
generator row order instead. We take the C++ order and drop `colorder`.
"""

import pathlib
import random
import re
import sys

N, K, M = 174, 87, 87
ROW_HEX = 22  # 22 hex chars = 88 bits, of which the first 87 are used


def parse_mn(cpp: str) -> list[list[int]]:
    body = cpp.split("BP_MAX_CHECKS>, N> Mn =")[1].split("}};")[0]
    nums = [int(x) for x in re.findall(r"-?\d+", body)]
    if len(nums) != N * 3:
        raise SystemExit(f"Mn: expected {N * 3} entries, found {len(nums)}")
    return [nums[i * 3 : (i + 1) * 3] for i in range(N)]


def parse_nm(cpp: str) -> list[list[int]]:
    body = cpp.split("std::array<CheckNode, M> Nm =")[1].split("}};")[0]
    entries = re.findall(r"\{\s*(\d+)\s*,\s*\{([^}]*)\}\s*\}", body)
    if len(entries) != M:
        raise SystemExit(f"Nm: expected {M} check nodes, found {len(entries)}")
    rows = []
    for count, values in entries:
        vals = [int(x) for x in re.findall(r"-?\d+", values)]
        rows.append(vals[: int(count)])
    return rows


def parse_generator(cpp: str) -> list[list[int]]:
    body = cpp.split("constexpr std::array<std::string_view, Rows> Data =")[1]
    rows = re.findall(r'"([0-9a-fA-F]{%d})"' % ROW_HEX, body)[:M]
    if len(rows) != M:
        raise SystemExit(f"generator: expected {M} rows, found {len(rows)}")
    out = []
    for row in rows:
        bits = bin(int(row, 16))[2:].zfill(ROW_HEX * 4)
        out.append([int(b) for b in bits[:K]])
    return out


def encode(info: list[int], gen: list[list[int]]) -> list[int]:
    """JS8 codeword: parity first, message second. Not systematic-prefix."""
    parity = [sum(info[j] * gen[i][j] for j in range(K)) % 2 for i in range(M)]
    return parity + list(info)


def verify(mn: list[list[int]], nm: list[list[int]], gen: list[list[int]]) -> None:
    edges_mn = {(c, b) for b, checks in enumerate(mn) for c in checks}
    edges_nm = {(c, b) for c, bits in enumerate(nm) for b in bits}
    if edges_mn != edges_nm:
        raise SystemExit("Mn and Nm describe different graphs — transcription is wrong")
    if any(len(c) != 3 for c in mn):
        raise SystemExit("Mn: column weight is not uniformly 3")
    print(f"  Mn/Nm agree: {len(edges_mn)} edges, column weight 3, "
          f"row weights {sorted({len(r) for r in nm})}")

    random.seed(20260729)
    for trial in range(500):
        info = [random.randint(0, 1) for _ in range(K)]
        cw = encode(info, gen)
        for i, bits in enumerate(nm):
            if sum(cw[b] for b in bits) % 2:
                raise SystemExit(
                    f"generator/parity mismatch: trial {trial} fails check {i}. "
                    "Upstream may have changed the generator row order — see "
                    "vendor/js8call/PROVENANCE.md."
                )
        if cw[M:] != info:
            raise SystemExit("message bits are not at cw[87..174]")
    print("  generator satisfies all 87 checks over 500 random words")


def emit(mn, nm, gen) -> str:
    lines = [
        "//! LDPC(174,87) tables for JS8 — **generated, do not edit**.",
        "//!",
        "//! Produced by `tools/gen_js8_ldpc.py` from the JS8Call commit recorded in",
        "//! `vendor/js8call/PROVENANCE.md`. Regenerate rather than patching by hand.",
        "//!",
        "//! The generator rows are taken in JS8Call's C++ order, which already has the",
        "//! Fortran `colorder` permutation folded in — so no separate reordering step",
        "//! exists here or in [`super::ldpc`].",
        "",
        "/// Codeword bits.",
        "pub const N: usize = 174;",
        "/// Information bits (72 message + 3 frame type + 12 CRC).",
        "pub const K: usize = 87;",
        "/// Parity checks.",
        "pub const M: usize = 87;",
        "/// Largest number of bits participating in any one parity check.",
        "pub const MAX_ROW: usize = 7;",
        "",
        "/// For each codeword bit, the three parity checks it participates in.",
        "/// Column weight is uniformly 3.",
        "pub const MN: [[u8; 3]; N] = [",
    ]
    for row in mn:
        lines.append("    [{:>3}, {:>3}, {:>3}],".format(*row))
    lines += [
        "];",
        "",
        "/// For each parity check, how many of its `NM` entries are valid.",
        "pub const NRW: [u8; M] = [",
    ]
    for chunk in (nm[i : i + 12] for i in range(0, M, 12)):
        lines.append("    " + " ".join(f"{len(r)}," for r in chunk))
    lines += [
        "];",
        "",
        "/// For each parity check, the codeword bits it sums over. Only the first",
        "/// `NRW[check]` entries of each row are meaningful; the rest are zero.",
        "pub const NM: [[u8; MAX_ROW]; M] = [",
    ]
    for row in nm:
        padded = row + [0] * (7 - len(row))
        lines.append("    [{:>3}, {:>3}, {:>3}, {:>3}, {:>3}, {:>3}, {:>3}],".format(*padded))
    lines += [
        "];",
        "",
        "/// Parity half of the systematic generator, bit-packed MSB-first: parity bit",
        "/// `i` is the GF(2) sum of the message bits selected by row `i`.",
        f"pub const GEN_PARITY: [[u8; {(K + 7) // 8}]; M] = [",
    ]
    for row in gen:
        packed = bytearray((K + 7) // 8)
        for j, bit in enumerate(row):
            if bit:
                packed[j >> 3] |= 0x80 >> (j & 7)
        lines.append("    [" + ", ".join(f"0x{b:02x}" for b in packed) + "],")
    lines.append("];")
    lines.append("")
    return "\n".join(lines)


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} <path-to-js8call-checkout>")
    src = pathlib.Path(sys.argv[1]).expanduser()
    cpp = (src / "JS8.cpp").read_text()

    mn, nm, gen = parse_mn(cpp), parse_nm(cpp), parse_generator(cpp)
    print("verifying tables against each other:")
    verify(mn, nm, gen)

    out = pathlib.Path(__file__).resolve().parent.parent / (
        "crates/sdroxide-digi/src/js8/ldpc_tables.rs"
    )
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(emit(mn, nm, gen))
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
