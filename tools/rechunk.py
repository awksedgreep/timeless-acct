#!/usr/bin/env python3
"""Re-store one collected data set at several chunk sizes and measure it.

The points are the collector's own, unchanged. Only how many of them share
a chunk varies, so the difference between rows is the cost of chunking and
nothing else. This is where the table in DESIGN.md, "Why every hour", came
from.

    TIMELESS_EXT=.../target/release/libtimeless_ext \
        tools/rechunk.py <store>/metrics.db <every> <size> [<size> ...]

<every> takes every Nth tick, to see a coarser interval in finer data.
The store is opened read-only; the copies are made in a temporary
directory and removed.
"""
import os, sqlite3, subprocess, sys, tempfile
from collections import defaultdict

# The timeless extension, without its suffix: build it in timeless-libsql
# with `cargo build --release -p timeless-ext`.
EXT = os.environ.get("TIMELESS_EXT") or sys.exit(
    "set TIMELESS_EXT to the path of libtimeless_ext, without .so")

def connect(path, ro=False):
    c = sqlite3.connect(f"file:{path}?mode=ro" if ro else path, uri=ro)
    c.enable_load_extension(True)
    c.load_extension(EXT)
    return c

def load(src, every=1):
    c = connect(src, ro=True)
    ticks = defaultdict(list)
    for name, labels, ts, value in c.execute(
            "SELECT name, labels, ts, value FROM metric_samples"):
        ticks[ts].append((name, ts, value, labels))
    order = sorted(ticks)[::every]
    return [ticks[t] for t in order]

def store(ticks, per_chunk, workdir):
    path = os.path.join(workdir, f"chunk{per_chunk}.db")
    for suffix in ("", "-wal", "-shm"):
        if os.path.exists(path + suffix):
            os.remove(path + suffix)
    c = connect(path)
    c.execute("CREATE VIRTUAL TABLE metric_samples USING timeless_metrics")
    for start in range(0, len(ticks), per_chunk):
        rows = [r for tick in ticks[start:start + per_chunk] for r in tick]
        c.executemany(
            "INSERT INTO metric_samples(name, ts, value, labels) VALUES (?,?,?,?)", rows)
        c.execute("INSERT INTO metric_samples(metric_samples) VALUES ('flush')")
        c.execute("INSERT INTO metric_samples(metric_samples) VALUES ('compact')")
        c.commit()
    points, chunks, ts_b, val_b, raw = c.execute(
        "SELECT sum(point_count), count(*), sum(length(ts_data)), sum(length(val_data)),"
        " sum(encoding = 1) FROM metric_samples_chunks").fetchone()
    series = c.execute("SELECT count(*) FROM metric_samples_series").fetchone()[0]
    c.close()
    # Table and index pages, which is what the file actually grows by.
    out = subprocess.run(
        ["sqlite3", path,
         "SELECT sum(pgsize) FROM dbstat WHERE name LIKE 'metric_samples_chunks%'"],
        capture_output=True, text=True, check=True).stdout.strip()
    return dict(per_chunk=per_chunk, points=points, chunks=chunks, series=series,
                ts=ts_b, val=val_b, raw=raw, pages=int(out))

def main():
    src, every = sys.argv[1], int(sys.argv[2])
    sizes = [int(s) for s in sys.argv[3:]]
    ticks = load(src, every)
    # Whole chunks only, so that no row is flattered or penalized by a stub.
    ticks = ticks[:len(ticks) // max(sizes) * max(sizes)]
    n = sum(len(t) for t in ticks)
    print(f"{len(ticks)} ticks, {n} points, taking every {every} tick(s)")
    print(f"{'pts/chunk':>9} {'chunks':>7} {'ts B/pt':>8} {'val B/pt':>9} "
          f"{'payload':>8} {'on disk':>8} {'B/chunk row':>11}")
    with tempfile.TemporaryDirectory(prefix="rechunk-") as workdir:
        for size in sizes:
            r = store(ticks, size, workdir)
            assert r["raw"] == 0, "uncompressed chunks left"
            assert r["points"] == n, (r["points"], n)
            payload = r["ts"] + r["val"]
            print(f"{size:>9} {r['chunks']:>7} {r['ts']/n:>8.2f} {r['val']/n:>9.2f} "
                  f"{payload/n:>8.2f} {r['pages']/n:>8.2f} "
                  f"{(r['pages']-payload)/r['chunks']:>11.1f}")

main()
