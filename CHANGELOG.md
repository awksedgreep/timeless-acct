# Changes

## 0.2.4

- A store in a standard place, so starting, watching, and stopping need
  nothing said about where (#25). `run` records into the user's own
  store, `~/.local/share/timeless-acct`, or as root into the host's,
  `/var/lib/timeless-acct`; `watch`, `top`, `exits`, and `trees` read the
  user's own if there is one and the host's otherwise. `--data-dir` and
  `TIMELESS_ACCT_DATA` still name another. The old default was
  `timeless-acct-data`, relative to wherever the command was run.

- The service keeps a local store in `/var/lib/timeless-acct`, where it
  had sent to the planes; it runs as a `timeless-acct` user of its own
  (`dist/timeless-acct.sysusers`) instead of `DynamicUser=`, and the
  members of the `timeless-acct` group may watch it. `dist/install.sh`
  installs, upgrades, and uninstalls it.

- A store's files are as private as its directory: `0600`, or `0640`
  where the directory lets its group in. A reader refused a store says so,
  and how to be let in, where it said the store did not exist.

- A stopped store can be read by someone who may not write beside it, a
  member of its group or a read-only mount: with no collector holding it,
  it is opened as immutable. Before, the reader was told it could not
  write a read-only database.

- A container image, `ghcr.io/awksedgreep/timeless-acct`, built from each
  version tag. It is a host agent in an image: rootful, with the host's
  PID, network, and cgroup namespaces and three capabilities. In a
  rootless container the kernel refuses exit accounting.

- The viewer's header has the version beside the name.

- The warning when the kernel refuses exit accounting is one sentence
  again, without a run of spaces in the middle.

- The README is a quick start; what was in it is in `docs/`: running,
  watching, reading, what is recorded, and what it costs.

## 0.2.3

- The store engine is timeless-libsql 0.8.9 (from 0.8.6). Compaction
  sweeps end, are planned once, and merge small chunks: on the live
  workstation store raw samples went from 399 to 1,527 points a chunk and
  from 0.38 to 0.18 bytes a sample, and the collector's memory from about
  325 to 215 MiB. Reads of a few series among many seek past the rest of
  the chunk index.

- The store-limit test follows the merged chunks: the eight hours of
  samples that can go are now under a twentieth of its store.

- `cargo fmt` and `cargo clippy` are clean again.

## 0.2.2

- What each tick costs on the wire is counted, so a store can show wire
  against stored: `acct_written_bytes_total{plane="metrics"|"logs"|"traces"}`,
  cumulative from the bodies the encoders render. `rate(acct_written_bytes_total[1h])`
  against `rate(acct_store_bytes[1h])` is the compression over any window. (#13)

- The Jobs view no longer reads two hundred trees to show one. Figures
  come from one scan of the reach; the selected row's tree is read for it
  alone. (#8)

- Correctness fixes: the queue counter increments before the send so a
  concurrent drain cannot leak capacity (#14); a failed `/proc` read keeps
  what tracking knew instead of attributing uid-0 defaults (#15); a
  delayed exit for an old pid incarnation no longer drops the live one's
  exec record, and exits sharing a pid in one batch no longer collapse
  (#16); `watch` and the store respect `--host` in history, timeline,
  incidents, exits, records, and jobs (#17); `exits` bounds the store's
  work with `LIMIT` and `trees` orders and limits in SQL (#18).

- Hardening: the bearer token is redacted from `Debug`, db and lease files
  are `0600`, error bodies are capped (#19); exec descriptions re-read
  `stat` to drop pid-reuse mismatches (#20); netlink checks short sends,
  reports truncation as overrun, widens the ACK window, validates the CPU
  list, and backs off error spam (#21).

- Performance: the exit wait is indexed by pid, OTLP grouping is
  single-pass, fd walks are gated on reportable processes, label caches
  are pruned (#22).

- The viewer: `check` shares one HTTP agent, hunt dedup is bounded, `go`
  clamps stale selections, shifting from live lands on the last stored
  moment, and time fields no longer byte-slice the clock (#23, #24).

## 0.2.4

- The viewer keeps the pace of the store. It took samples to be ten
  seconds apart: a store sampled less often than every thirty seconds
  had an empty screen at most moments in the past, and one sampled every
  second was stepped through ten samples at a time.

- Looking for something in the viewer no longer stops the screen or
  takes gigabytes. Over an hour in which a quarter of a million processes
  ended, `/rustc` took 4 seconds among the exits and 25 among the jobs,
  and left the viewer at 2.85 GB; it takes half a second and under two,
  and the viewer is at 160 MB after. (#4, #5)
- A job is found by anything that ran in it, and not only by what it was
  started with. (#7)
- `esc` goes back from what is looked for before it leaves the viewer.
  (#6)

- The local store keeps less, and the same as a node: samples for 7 days
  (was 30), rolled up to five minutes for 30 days and to an hour for 180
  (was 180 days and forever), records for 30 days (was 90), spans for 30.
  A week of samples is enough to fight a fire with, six months has its
  value at the hour, and nothing is kept forever, because a forever tier
  holds every process that ever lived. They apply to a store when it is
  created; an existing store keeps what it was made with. (#10)

- A limit on what the local store may hold: `--store-limit`, 2 GiB
  unless told otherwise. Over it, the collector prunes the oldest of the
  least valuable kind at each maintenance pass, samples first and records
  last, never into the last hour. The store's size and limit are
  `acct_store_bytes` and `acct_store_limit_bytes`. (#11)
- `acct_interval_seconds` and `acct_process_interval_seconds` say how
  often the collector was told to look, so a reader can choose its
  lookback from the store. (#9)
- The engine is the one with series removed by retention, rollup chunks
  merged, and a changed window applied at the next pass
  (timeless-libsql `5bca399`). With the defaults, a series now goes with
  its hourly rollup after 180 days instead of staying forever.

- The viewer's day-long timeline tells both ends with their day. It
  read `21:12` at both, since the day was added only past a day. (#12)

## 0.2.1

- After a burst of processes the collector comes back to near the size
  it was. It had stayed at more than twice that: its maps and SQLite's
  page cache kept the room the burst had needed.

## 0.2.0

What is stored has changed shape in two places. A store written by 0.1.0
can be written to by 0.2.0, and keeps what it has the way it was written.

- **A span's service is `host/unit`**, where it was the unit. Two hosts
  that push to one traces plane no longer share their services.
  `trees --unit` looks for `host/unit`, and takes `--host`; it does not
  find the spans 0.1.0 wrote.
- **Every `unit_*` series has a `kind`**: `service`, `scope`, `slice`, or
  `manager`. It is a new label, so a unit's series begin again. (#1)

And what is not a change of shape:

- The queues between the listeners and the collector take memory as they
  fill, and hold eight and sixteen times what they did. 0.1.0 lost 7,850 exit
  records in two minutes of a large build, and 103,794 of 120,000 in a
  test; 0.2.0 lost none of either.
- Memory and disk are given back. The write-ahead logs are truncated
  after maintenance and limited to 8 MB, and what the allocator holds
  free is returned after every flush. A store of 47 MB had 95 MB of logs
  beside it, and the collector stayed at the size of its largest moment.
- The README says what the collector's memory is made of, and that a
  PromQL reader should pass `lookback_delta`. (#2)
- DESIGN.md no longer says a canvas element has at most one label. (#3)

## 0.1.0

The first version: the collector, both sinks, the viewer, and `top`,
`exits`, and `trees`.
