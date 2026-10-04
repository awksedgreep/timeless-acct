# Reading a store

Three commands answer from a store without a screen: `top`, the processes
of a moment; `exits`, the processes that ended; and `trees`, jobs as the
trees of processes they ran. Like `watch`, they read your own store if
you have one and the host's otherwise, or the one `--data-dir` names.

```sh
timeless-acct top --at -15m                     # a quarter of an hour ago
timeless-acct top --at "2026-09-29 03:12" --sort rss -n 10
timeless-acct exits --since -1d --status SIGKILL
timeless-acct exits --since 09:00 --comm rustc
timeless-acct exits --since -1d --unit mark/timeless-stack.service
timeless-acct exits --since -1h --summary       # by command, as sa(8) does
timeless-acct exits --since -1h --summary --by unit
timeless-acct trees --since -1h                 # jobs, as trees
timeless-acct trees --comm rustc --failed       # builds in which something failed
timeless-acct trees --unit mark/timeless-stack.service --width 0
timeless-acct trees --unit caddy.service --host web-2   # of a store that holds another host's
```

```text
   COUNT  FAILED        CPU    ELAPSED  PEAK RSS       READ    WRITTEN  COMMAND
     113       0      5m33s      1m41s   1.2 GiB   16.0 KiB    215 MiB  rustc
      79       0      13.4s      13.6s   340 MiB        0 B        0 B  cc1
     762       0       2.6s       2.9s   7.3 MiB        0 B        0 B  hyprctl
     753       0       2.4s       2.4s  22.8 MiB        0 B        0 B  postgres
    8324     108      6m10s      2d15h   1.2 GiB   19.9 MiB    873 MiB  (all 104 commands)
```

That is ten minutes of one workstation: 8,324 processes ended, and most of
them lived for a few milliseconds.

Times are `now`, a distance back (`-90s`, `-15m`, `-2h`, `-1d`), a local
time today (`14:30`), a local date and time, or epoch seconds.

These read what has been flushed. A running collector holds up to a minute
of samples in memory that are not visible yet.

Or use SQL, with the timeless extension loaded:

```sql
.load libtimeless_ext
SELECT ts, value FROM metric_samples
 WHERE name = 'proc_rss_bytes' AND json_extract(labels, '$.comm') = 'postgres';

SELECT message FROM logs WHERE service = 'postgres' AND status = 'SIGKILL';
```

The store is three databases, one for each kind of data, laid out as the
planes lay out theirs. One query can read all three: open one and attach
the others.

```sh
sqlite3 'file:/var/lib/timeless-acct/metrics.db?mode=ro'
```

```sql
.load libtimeless_ext
ATTACH 'file:/var/lib/timeless-acct/logs.db?mode=ro' AS logs;
ATTACH 'file:/var/lib/timeless-acct/traces.db?mode=ro' AS traces;

-- Each unit at a moment: its CPU, and how many of its processes ended in
-- the minute that followed, and how many of those failed.
WITH cpu AS (
  SELECT json_extract(labels, '$.unit') AS unit, value AS cpu_pct
    FROM timeless_latest('metric_samples', 'unit_cpu_pct', NULL, 1790723880, 1790723910)
), ended AS (
  SELECT json_extract(metadata, '$.unit') AS unit,
         count(*) AS ended, sum(status <> '0') AS failed
    FROM logs.logs
   WHERE ts BETWEEN 1790723880000000 AND 1790723940000000
   GROUP BY 1
)
SELECT unit, round(cpu_pct, 1) AS cpu_pct, coalesce(ended, 0) AS ended, coalesce(failed, 0) AS failed
  FROM cpu LEFT JOIN ended USING (unit)
 ORDER BY cpu_pct DESC LIMIT 10;
```

Samples are in epoch seconds, records in microseconds, and spans in
nanoseconds, as the planes keep them. A span's `service` is `host/unit`.
Opening read-only (`mode=ro`) is what lets this run beside a collector
that owns the store.
