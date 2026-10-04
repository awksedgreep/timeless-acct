# Running the collector

The [quick start](../README.md#getting-started) installs the collector as
a service and needs nothing more. This page is what it does, and the
choices it leaves open.

## The service

`sudo dist/install.sh` puts three things in place, and starts the service:

| | |
|---|---|
| `/usr/local/bin/timeless-acct` | the binary |
| `/etc/sysusers.d/timeless-acct.conf` | a `timeless-acct` user and group |
| `/etc/systemd/system/timeless-acct.service` | [the unit](../dist/timeless-acct.service) |

The service runs as the `timeless-acct` user, holding the three
capabilities it uses and no others, and records into
`/var/lib/timeless-acct`. Members of the `timeless-acct` group may read
that store; the script adds whoever ran it through `sudo`, and others are
added with `sudo usermod -aG timeless-acct NAME`. A new member is a member
from their next login.

```sh
sudo systemctl stop timeless-acct       # stop recording
sudo systemctl start timeless-acct      # and go on, in the same store
sudo systemctl status timeless-acct
journalctl -u timeless-acct             # what it said: what it pruned, what it was refused
```

Stopping flushes what it holds, so nothing is lost but the time it was
stopped. To upgrade, build again and run `sudo dist/install.sh` again: it
restarts the service on the new binary. `sudo dist/install.sh --uninstall`
stops and removes the service and the binary, and keeps the store, the
user, and the group.

The store holds the command line of every process that ran on the host.
Whoever can read it can read those: see [Secrets](recorded.md#secrets).

To change what the service does, edit its `ExecStart` with
`sudo systemctl edit --full timeless-acct`, using the options below.

## By hand

```sh
timeless-acct run          # Ctrl-C stops it
timeless-acct watch        # in another terminal
```

Run by a user, the collector records into that user's own store,
`~/.local/share/timeless-acct` (under `$XDG_DATA_HOME` if it is set); run
by root, into the host's, `/var/lib/timeless-acct`.

Without privileges it still runs, and misses what they would show: see
[Privileges](#privileges). To give a binary run by hand all of them:

```sh
sudo setcap cap_net_admin,cap_sys_ptrace,cap_dac_read_search+ep $(which timeless-acct)
```

A new binary does not keep them, so this is done again after each build.
The collector says at its start what it was refused.

## In a container

On a host whose services all run as containers, the collector can be one
too: `ghcr.io/awksedgreep/timeless-acct`, built from each version tag. It
is a host agent in an image, not a contained application, and needs what
the service needs from the host:

```sh
sudo podman run -d --name timeless-acct \
  --pid=host --network=host --cgroupns=host --uts=host \
  --cap-drop=all --cap-add=NET_ADMIN,SYS_PTRACE,DAC_READ_SEARCH \
  --read-only \
  -v /etc/passwd:/etc/passwd:ro -v /etc/group:/etc/group:ro \
  -v /var/lib/timeless-acct:/var/lib/timeless-acct \
  ghcr.io/awksedgreep/timeless-acct:0.2.4

sudo podman exec -it timeless-acct timeless-acct watch
```

| | why |
|---|---|
| a rootful runtime | exit accounting is offered only to the initial user namespace: in a rootless container, or with user namespaces remapped, the kernel refuses it, and the processes that end between samples are missed |
| `--network=host` | the kernel's accounting and exec families are in the initial network namespace only |
| `--pid=host` | every process, under the PIDs its exit records carry |
| `--cgroupns=host` | units under their names, not `/` |
| `--uts=host` | the host's name, not the container's |
| the three capabilities | as the service: see [Privileges](#privileges) |
| `/etc/passwd`, `/etc/group` | users and groups by name |

`timeless-acct check` in the same container says whether it got them.

## Where the store is

| who runs it | where it records |
|---|---|
| the service, or root | `/var/lib/timeless-acct` |
| a user | `~/.local/share/timeless-acct` |

`watch`, `top`, `exits`, and `trees` read the user's own store if they
have one, and the host's otherwise. `--data-dir`, or the
`TIMELESS_ACCT_DATA` environment variable, names another, for writing and
for reading:

```sh
timeless-acct watch --data-dir /srv/copies/web-1
```

A store is a directory of three SQLite databases, one for samples, one for
accounting records, and one for spans; see
[reading a store](reading.md) for reading them with SQL. One collector
owns a store at a time, and a second is refused; any number may read it
while it does.

A new store is its owner's alone to read. A store whose directory lets its
group in, as the service's does, is its group's to read as well. No one
else's, ever.

## What this host lets it see

```sh
timeless-acct check
```

`check` reports what this host and this user let the collector see, and
what to do about what they do not:

```text
exit accounting              refused: needs CAP_NET_ADMIN
delay accounting             off
pressure stall information   available
process I/O                  visible for 114 of 521 processes
scheduler wait               visible for 521 of 521 processes

metrics plane                http://127.0.0.1:8428  answering, version 0.8.5
logs plane                   http://127.0.0.1:9428  answering, version 0.8.5
```

## Privileges

Without any, the collector still runs. Each capability adds something:

| capability | adds |
|---|---|
| none | system statistics; units; CPU and memory of every process; I/O and open files of the user's own |
| `CAP_NET_ADMIN` | exit records from the kernel: every process, however short-lived, and what each was running |
| `CAP_SYS_PTRACE` and `CAP_DAC_READ_SEARCH` | I/O, open files, and executable path of other users' processes. It takes both |

[The unit](../dist/timeless-acct.service) runs the collector as an
unprivileged user holding exactly those three. A binary run by hand is
given them with `setcap`: see [by hand](#by-hand).

Exit accounting is offered only to the initial user and PID namespaces: not
inside a container, and not under `PrivateUsers=`.

Delay accounting (time spent waiting for a CPU, for block I/O, for memory)
is compiled into most kernels and off by default. Turn it on with
`sysctl kernel.task_delayacct=1`.

## How often

Every ten seconds, unless told otherwise: sar's default is every ten
minutes.

```sh
timeless-acct run --interval 1 --process-interval 1     # every second
timeless-acct run --interval 10 --process-interval 60   # processes by the minute
```

`--interval` is how often the system is read and `--process-interval` how
often processes and units are; either may be any whole number of seconds
from one up. Exit records are not affected: every process that ends is
recorded whatever the interval, as it happens.

A rate is taken over the interval, so a shorter one catches what a longer
one averages away. Ten seconds of a core at 100% is 100% sampled every ten
seconds, and 17% sampled every minute. On one workstation over the same
four minutes, a browser's highest CPU was 134% sampled every second and
32% sampled every forty. When a process pins a CPU for a few seconds,
every second is what finds it, and the rewind then shows which process it
was.

What it costs:

| every | collector CPU, measured on that workstation | samples, against every ten seconds |
|---|---|---|
| 1 s | 5% of one CPU | ten times as many |
| 10 s | 0.65% | |
| 40 s | 0.4% | a quarter |

Storage follows the samples, though not one to one; what a sample costs
is [measured below](cost.md#what-a-sample-costs-to-store) at ten seconds only.

The viewer keeps pace with whatever the store was sampled at: a step in
time is one sample, and a series is on the screen if it has one in the
last three. Two things do not adjust themselves: `top --within`, which is
sixty seconds unless told, and a PromQL reader's lookback, which should
be two or three times the interval (see
[Reading with PromQL](#reading-with-promql)).

## Options

`timeless-acct run --help` lists them all. The ones that matter:

| option | default | |
|---|---|---|
| `--sink` | `embedded` | `embedded`, `http`, or `stdout` |
| `--host` | the hostname | the name this host is recorded under |
| `--interval` | 10 | seconds between readings of the system, from 1 |
| `--process-interval` | 10 | seconds between sweeps of the processes, from 1 |
| `--min-age` | 30 | seconds a process must have lived to get series of its own |
| `--kernel-threads` | off | give kernel threads series of their own |
| `--no-units` | | do not report units |
| `--no-exec-events` | | do not ask the kernel for word of each exec |
| `--no-traces` | | do not keep a span for each process that ends |
| `--trace-max-age` | `1h` | how long after a job starts a process may start and be part of its trace |
| `--retention` | `7d` | local store: how long samples are kept |
| `--rollups` | `5m@30d,1h@180d` | local store: coarser copies kept after that |
| `--log-retention` | `30d` | local store: how long accounting records are kept |
| `--trace-retention` | `30d` | local store: how long spans are kept |
| `--store-limit` | `2G` | local store: what it may hold on disk; `0` for no limit |
| `--token` | | planes: a bearer token, if they require one |

Sampling faster or slower, and what it costs, is under [How
often](#how-often).

Retention and rollups apply when a store is created; a store made with
other settings keeps them, and there is no message. Making the flags the
store's settings on every start needs the engine to take a new window
for samples and spans as it does for records
([timeless-libsql#91](https://github.com/awksedgreep/timeless-libsql/issues/91)).
For the planes, the windows are the planes' own settings, and the same
ones.

**The limit** is what keeps a store from filling a disk whatever the
windows say. At each maintenance pass the collector measures the three
databases and their logs, less the pages SQLite has freed, and while they
are over the limit it prunes the oldest of the least valuable kind:
samples first, then spans, then rollups finest first, then records. The
last hour of anything is never pruned; if the store is still over the
limit with only that, the collector says so and goes on. Pruning is
chunk-granular, so it lands a little under the limit, and the store can
be over it by an hour's writes between passes. What was pruned is in the
log, one line a kind, and the store's size and limit are among the
`acct_*` gauges. A rollup tier that is pruned is given a shorter window,
and keeps it.

Why these: a week of samples is enough to fight a fire with, and samples
are most of the bytes. Six months has its value at one resolution, the
hour: was the box this busy in March, when did this service's memory
start climbing. Anything finer for that long is bytes for questions
nobody asks. Nothing is kept forever, because a forever tier holds every
process that ever lived. Records are what `exits` searches back through
at about 40 bytes each; a quiet server can afford `--log-retention 90d`.

If a plane is unreachable, the collector keeps up to an hour of ticks and
sends them, in order and at the times they were taken, when it answers.

## To the canvas

The canvas reads from the Timeless planes. Point the collector at them,
on the same network: this sends every sample as text, about 100 bytes
each, and is not the path for a remote collector on a thin link.

```sh
timeless-acct run --sink http
# --metrics-url http://127.0.0.1:8428 --logs-url http://127.0.0.1:9428
# --traces-url http://127.0.0.1:10428
```

Then, on a canvas, an element for a service, a container, or an
application is:

| field | value |
|---|---|
| host | `web-1` |
| metric | `unit_cpu_pct` |
| series label | `unit` = `postgresql.service` |

and for one process:

| field | value |
|---|---|
| host | `web-1` |
| metric | `proc_cpu_pct` |
| series label | `proc` = `postgres[1234]` |

A unit is the same line after a restart. A process is a new one, because
it is a new process. The timeline scrubber does the rest.

These are the shortest way to name a line, and not the only one. Any label
can be a field of an element: `comm` = `postgres` beside `proc_cpu_pct` is
every postgres process, and `kind` = `service` beside a `unit_*` metric is
every unit that does not hold other units.

### Reading with PromQL

Pass `lookback_delta` of two or three times `--process-interval`: `30s`,
for the default of ten seconds.

```sh
curl -G http://127.0.0.1:8428/api/v1/query \
  --data-urlencode 'query=sum by (comm) (proc_rss_bytes)' \
  --data-urlencode 'lookback_delta=30s'
```

PromQL takes the last sample of each series within its lookback, which is
five minutes unless it is told otherwise. A process that has ended writes
no more samples, and its last one goes on being its value until the
lookback has passed it. Anything that adds up or ranks the `proc_*` tier
counts the dead among the living for that long: on one workstation, the
memory of a browser that restarts its processes was 7.7 GB by the default
and 4.7 GB by `30s`, and the count of processes was 207, where the
collector had reported 201.

A lookback shorter than the interval finds nothing between two samples.
The viewer uses three of the store's samples, however far apart they
are; `timeless-acct top` takes `--within`, and sixty seconds unless told.

A host element turns red when a process on the host dies of a fault, and
amber when one is killed; see
[the level of a record](../DESIGN.md#the-level-is-a-judgement-about-the-host).

## Serving a store to a canvas

A store is laid out as the planes lay out theirs, so it can be served to
a canvas later, by the planes' own servers:

```sh
timeless-metrics-api libtimeless_ext.so /var/lib/timeless-acct/metrics.db
TIMELESS_LOGS_TIMESTAMP_UNIT=us \
  timeless-logs-api libtimeless_ext.so /var/lib/timeless-acct/logs.db
timeless-traces-api libtimeless_ext.so /var/lib/timeless-acct/traces.db
```

One owner at a time: a server and a collector cannot both hold a store, and
whichever comes second is refused.
