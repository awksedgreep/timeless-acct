# Changes

## Not yet in a version

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
