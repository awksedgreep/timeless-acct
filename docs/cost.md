# What it costs

Measured at the default intervals, on a 22-CPU workstation running a
desktop, a browser, a dozen containers, and a Rust build: about 530
processes, 190 of them old enough for series of their own, 74 units, and
between 13 and 30 processes ending every second.

| | |
|---|---|
| collector CPU | 0.65% of one CPU. A sweep of 530 processes and 74 units takes 25 ms |
| collector memory | 60 to 115 MB resident with 11,000 series in the store, and more with more: see [below](#what-the-store-costs-in-memory) |
| samples | 4,800 a tick: 2,600 of processes, 1,100 of units, 700 of totals, 400 of the system |
| on the wire, to the planes | 470 KB a tick, uncompressed |
| accounting records, stored | 33 to 39 bytes each, from 760 to 1,050 before compression |
| spans, stored | 51 bytes each, from 820 |

A day at that rate is 41 million samples, and between one and two and a
half million accounting records and as many spans.

## What it adds up to

How much smaller stored than written:

| | written | stored | smaller by |
|---|---:|---:|---:|
| a sample, as sent to the planes | about 100 bytes of text | 1.1 to 3.5 bytes | 30 to 90 times |
| a sample, as a timestamp and a value | 16 bytes | 1.1 to 3.5 bytes | 5 to 15 times |
| an accounting record | 760 to 1,050 bytes | 33 to 39 bytes | 20 to 30 times |
| a span | 820 bytes | 51 bytes | 16 times |

And what that is on the workstation above, at the defaults, from the
first 26 hours: one working day, with a large build in it, and a night.
A server that ends fewer processes will be well under it.

| | a day | kept for | levels off at |
|---|---:|---:|---:|
| samples, every ten seconds | about 60 MB | 7 days | about 0.4 GB |
| samples, rolled up to five minutes | about 40 MB | 30 days | about 1.2 GB |
| samples, rolled up to an hour | about 23 MB | 180 days | about 4 GB |
| the series catalog | 5 to 15 MB | with the last rollup | 1 to 2.6 GB |
| accounting records | 34 to 63 MB | 30 days | 1 to 1.9 GB |
| spans | 49 to 92 MB | 30 days | 1.5 to 2.8 GB |
| **all of it** | **about 200 to 300 MB** | | **about 9 to 12 GB, at 180 days** |

The rollups cost more than the samples they are made of, at one small
chunk per series per hour, and the catalog was never emptied. Both are
fixed in the engine and not yet released
([timeless-libsql#81](https://github.com/awksedgreep/timeless-libsql/issues/81),
[#82](https://github.com/awksedgreep/timeless-libsql/issues/82)): with
them the rollup rows are about half, and a series goes with its last
rollup instead of staying. Until then the catalog row keeps growing.

For a sense of scale: a week of every process on a busy workstation is
about 2 GB, and its samples alone 0.4.

## What the store costs in memory

With a store of its own, the collector's memory is mostly the engine's
index of that store, and grows with it.

| | |
|---|---|
| a series in the store | about 1.4 KB, and the engine keeps a series after its samples are gone |
| a chunk | about 225 bytes; a flush writes one for each series with samples, which was 1.2 MB a minute, until compaction merges them |
| a series that has ended, with the two or three chunks left of it | about 1.9 KB |
| a maintenance pass | up to 130 MB while it runs, given back when it ends |

A process that lives for thirty seconds is fifteen series, so the number
of series is the number of processes there have been, and not the number
there are. The workstation above made between 1,000 and 4,000 series an
hour. At 1.9 KB each that is 50 to 180 MB a day. With the engine as
released it does not level off: retention removes a series' data and
leaves the series. With
[timeless-libsql#82](https://github.com/awksedgreep/timeless-libsql/issues/82)
a series goes with its last rollup chunk, which at the defaults is after
180 days: 3 to 10 GB of memory at the most, on this workstation, and a
tenth of that on a server that ends few processes.

Half the series of that store never held a value but zero: a process that
never swapped, never faulted a page in from disk, never read or wrote.
They are stored all the same, so that a reader is told nothing happened
and does not have to infer it.

Pushing to the planes moves all of this to the planes.

## What a sample costs to store

It depends mostly on how many samples share a chunk, and much less on the
samples. One hour of collected data, 1.4 million samples, re-stored with
only the chunk size changed:

| samples in a chunk | bytes a sample, on disk |
|---:|---:|
| 30 | 6.28 |
| 60 | 3.01 |
| 120 | 1.69 |
| 360 | 1.09 |

A chunk costs about 155 bytes whatever it holds. A local store compacts
every hour (`--maintain-interval`), which is 360 samples to a chunk and the
last row. [DESIGN.md](../DESIGN.md#why-every-hour) has the measurements and
the reasoning.

Each series also costs about 250 bytes once, for its name and labels, and
each compaction writes one chunk per series per rollup tier.

What moves the cost, in order: `--maintain-interval` for a local store,
`--process-interval` (the process and unit tiers are nine tenths of the
samples), `--min-age` (fewer processes with series of their own), and
`--retention`.

Through the planes, chunking is the planes' own: they compact every five
minutes.
