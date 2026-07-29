#!/usr/bin/env python3
"""Generate `crates/hive-cloud/assets/geoloc.bin` -- the LOCAL prefix->coordinate
table Seer's geo-DNS (`crates/hive-cloud/src/geoip.rs`) uses instead of calling a
third-party geolocation service on the DNS data path.

    scripts/gen-geo-table.py dbip-city-lite-YYYY-MM.csv.gz \\
        crates/hive-cloud/assets/geoloc.bin [--tol-km=800]

Source data
-----------
DB-IP "IP to City Lite", published monthly at
https://download.db-ip.com/free/dbip-city-lite-<YYYY-MM>.csv.gz under CC-BY-4.0.
CC-BY permits the redistribution this script performs; MaxMind's GeoLite2 is
otherwise equivalent but its licence does not, which is why it is not the input.
Attribution lives in `crates/hive-cloud/assets/geoloc.bin.README`.

This script is NOT part of the build -- the blob it emits is committed, so a node
builds and runs with zero network access and zero third-party runtime calls.
Refreshing is a deliberate act: re-run this, commit the new blob, roll the fleet
the way any other binary change rolls. The data ages slowly (prefix->city
assignments move on the order of months) and a stale table degrades to "slightly
wrong nearest node", never to an outage.

Why the output is ~5 MB and not ~90 MB
--------------------------------------
The raw dataset is 8.05M ranges at city precision. Seer does not need city
precision: it only ranks node sites that are thousands of km apart. Two
reductions, both measured against the raw data (numbers repeated in geoip.rs):

  1. Coordinates are quantised to a 1-degree grid cell (0..64799, fits a u16).
     Median error ~48 km -- far below the spacing of any two node sites.
  2. Adjacent runs are folded together when folding moves the answer less than
     --tol-km. Error is measured against the KEPT anchor, never the running
     value, so a folded chain cannot drift past the tolerance.

At the default 800 km tolerance: 8.05M ranges -> ~0.92M rows, costing ~18 km of
extra great-circle distance per query against a hypothetical 24-site global
fleet (~1 km against the current 5-metro one). A 1500 km tolerance would save
1.7 MB and cost 89 km; a 400 km tolerance costs 2.3 MB to save 5 km. 800 is the
knee, and the knee is why this is a flag and not a constant.

Output format (little-endian, version 1)
----------------------------------------
    0   magic            8   b"HIVEGEO\\x01"
    8   grid_deg_tenths  u16  10  (1.0 degrees)
    10  grid_nlon        u16  360
    12  grid_nlat        u16  180
    14  reserved         u16  0
    16  n4               u32  IPv4 row count
    20  n6               u32  IPv6 row count
    24  fnv1a64(rows)    u64  checksum over every row byte
    32  rows4            n4 * 6   (start u32, cell u16), ascending by start
        rows6            n6 * 6   (start u32 = top 32 bits of the v6 address)

A row means "from `start` until the next row's start, the location is `cell`".
`cell == 0xFFFF` marks a span the source data cannot place -- an explicit hole,
so a lookup never inherits a neighbour's coordinates by accident.
"""

import gzip
import math
import struct
import sys

UNK = 0xFFFF
DEG = 1.0
NLON = int(round(360 / DEG))
NLAT = int(round(180 / DEG))
assert NLON * NLAT <= UNK, "grid must be addressable in a u16 with 0xFFFF spare"

# v6 rows are keyed on the top 32 bits (a /32 -- one RIR allocation to one ISP).
# Coverage inside a key is weighted in /48s, so a big sub-block outvotes a small
# one instead of whichever happened to be listed first.
V6_KEY_BITS = 32
V6_KEY_SHIFT = 128 - V6_KEY_BITS
V6_WEIGHT_SHIFT = 128 - 48
V6_UNITS_PER_KEY = 1 << (V6_KEY_SHIFT - V6_WEIGHT_SHIFT)


def cell_of(lat, lon):
    li = int((lat + 90.0) // DEG)
    lo = int((lon + 180.0) // DEG)
    li = 0 if li < 0 else (NLAT - 1 if li >= NLAT else li)
    lo = 0 if lo < 0 else (NLON - 1 if lo >= NLON else lo)
    return li * NLON + lo


def center_of(cell):
    if cell == UNK:
        return None
    li, lo = divmod(cell, NLON)
    return (li * DEG - 90.0 + DEG / 2, lo * DEG - 180.0 + DEG / 2)


def haversine_km(a, b):
    r = 6371.0
    p1, p2 = math.radians(a[0]), math.radians(b[0])
    dp = p2 - p1
    dl = math.radians(b[1] - a[1])
    x = math.sin(dp / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(dl / 2) ** 2
    return 2 * r * math.asin(min(1.0, math.sqrt(x)))


def ip4(s):
    a, b, c, d = s.split(".")
    return (int(a) << 24) | (int(b) << 16) | (int(c) << 8) | int(d)


def ip6(s):
    if "::" in s:
        left, right = s.split("::", 1)
        lp = [x for x in left.split(":") if x]
        rp = [x for x in right.split(":") if x]
        parts = lp + ["0"] * (8 - len(lp) - len(rp)) + rp
    else:
        parts = s.split(":")
    v = 0
    for p in parts:
        v = (v << 16) | int(p or "0", 16)
    return v


def parse(path):
    """Stream the CSV into two run lists. Source ranges arrive sorted and
    disjoint; a hole between two ranges becomes an explicit UNK run."""
    runs4, runs6 = [], []
    end4 = None
    cur6 = None       # /32 key currently accumulating
    dom6 = {}         # cell -> /48s of coverage inside cur6
    end6 = None       # highest /32 key any range has covered

    def emit(runs, start, cell):
        if runs and runs[-1][1] == cell:
            return
        if runs and runs[-1][0] == start:
            runs[-1] = (start, cell)
            return
        runs.append((start, cell))

    def flush6():
        nonlocal cur6
        if cur6 is None:
            return
        # Ties break on the lower cell id so the output is byte-reproducible.
        emit(runs6, cur6, max(dom6.items(), key=lambda kv: (kv[1], -kv[0]))[0])
        dom6.clear()
        cur6 = None

    def take6(key, cell, weight):
        nonlocal cur6
        if cur6 is not None and cur6 != key:
            prev = cur6
            flush6()
            if key > prev + 1:
                emit(runs6, prev + 1, UNK)
        if cur6 is None:
            cur6 = key
        dom6[cell] = dom6.get(cell, 0) + weight

    with gzip.open(path, "rt", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            f = line.rstrip("\n").split(",")
            if len(f) < 8:
                continue
            try:
                lat = float(f[-2])
                lon = float(f[-1])
            except ValueError:
                continue
            # DB-IP marks unplaceable space with country ZZ; (0,0) -- null
            # island -- is its other spelling of "no data", never a real site.
            cell = UNK if (f[3] == "ZZ" or (lat == 0.0 and lon == 0.0)) else cell_of(lat, lon)
            start, stop = f[0], f[1]
            if ":" in start:
                a, b = ip6(start), ip6(stop)
                wa, wb = a >> V6_WEIGHT_SHIFT, b >> V6_WEIGHT_SHIFT
                ka, kb = a >> V6_KEY_SHIFT, b >> V6_KEY_SHIFT
                per_key = V6_UNITS_PER_KEY
                if ka == kb:
                    take6(ka, cell, wb - wa + 1)
                    end6 = ka if end6 is None else max(ka, end6)
                    continue
                # Only the first and last key of a range can be partly covered;
                # every key between them belongs to this range outright.
                take6(ka, cell, ((ka + 1) * per_key) - wa)
                if kb > ka + 1:
                    flush6()
                    emit(runs6, ka + 1, cell)
                take6(kb, cell, wb - (kb * per_key) + 1)
                end6 = kb if end6 is None else max(kb, end6)
            else:
                a, b = ip4(start), ip4(stop)
                if end4 is not None and a > end4 + 1:
                    emit(runs4, end4 + 1, UNK)
                emit(runs4, a, cell)
                end4 = b if end4 is None else max(b, end4)
    flush6()
    # Close both tables at the top of their key space. Without a trailing UNK,
    # an address above everything the source covers would inherit the LAST row's
    # coordinates -- a confidently wrong answer where "unknown" is the truth.
    if end4 is not None and end4 < 0xFFFFFFFF:
        emit(runs4, end4 + 1, UNK)
    if end6 is not None and end6 < 0xFFFFFFFF:
        emit(runs6, end6 + 1, UNK)
    return runs4, runs6


def simplify(runs, tol_km):
    """Fold a run into the previous KEPT run when doing so moves the answer less
    than `tol_km`. Comparing against the kept anchor rather than the previous
    input run is what bounds accumulated drift at exactly tol_km."""
    out = []
    for start, cell in runs:
        if out:
            prev = out[-1][1]
            if prev == cell:
                continue
            if prev != UNK and cell != UNK and haversine_km(center_of(prev), center_of(cell)) <= tol_km:
                continue
        out.append((start, cell))
    return out


def fnv1a64(buf):
    h = 0xCBF29CE484222325
    for b in buf:
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    tol = 800.0
    for a in sys.argv[1:]:
        if a.startswith("--tol-km="):
            tol = float(a.split("=", 1)[1])
    if len(args) != 2:
        print(__doc__)
        return 2
    src, dst = args
    runs4, runs6 = parse(src)
    t4 = simplify(runs4, tol)
    t6 = simplify(runs6, tol)
    rows = bytearray()
    for start, cell in t4:
        rows += struct.pack("<IH", start, cell)
    for start, cell in t6:
        rows += struct.pack("<IH", start, cell)
    head = struct.pack(
        "<8sHHHHIIQ", b"HIVEGEO\x01", int(DEG * 10), NLON, NLAT, 0, len(t4), len(t6), fnv1a64(rows)
    )
    assert len(head) == 32, len(head)
    with open(dst, "wb") as fh:
        fh.write(head)
        fh.write(rows)
    print(
        f"{dst}: v4 {len(runs4)} -> {len(t4)} rows, v6 {len(runs6)} -> {len(t6)} rows, "
        f"{len(head) + len(rows)} bytes (tol {tol:g} km)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
