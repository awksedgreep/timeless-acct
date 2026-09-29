# Changes

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
