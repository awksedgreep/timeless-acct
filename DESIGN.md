# Design

Why timeless-acct is shaped the way it is. The README says what it does;
this says what was decided, and what the decision cost.

## The goal

Put a process on a Timeless canvas, watch it, and drag the timeline back to
see what it was doing last Tuesday. For every process on the host, and for
the host itself, with nothing unaccounted for.

Three things follow from that sentence, and each one decided part of the
design.

## 1. The canvas decides the shape of a metric

A canvas element names a line most easily by **host, metric name, and one
more label**, and it draws **the last value in each time bucket**. It treats
a series as a counter only if it carries SNMP-style type metadata, which
nothing pushed over the Prometheus import route does.

One more label is what is easiest, and not all an element can have. Every
field of an element that does not configure it is sent as a label filter,
choosing a series from the list writes all of that series' labels into the
element, and a canvas variable adds its own. An earlier version of this
document said "at most one", and was wrong.

So:

**Every metric is a gauge, with the rate already taken.** A raw kernel
counter (`utime` ticks, bytes read) would draw as a line that only rises,
and rewinding to it would show a number that means nothing. sar has always
recorded rates for the same reason. `proc_cpu_seconds` is the one cumulative
figure kept, because "how much CPU has this process used in its life" is an
accounting question in its own right.

The cost: a rate is fixed at the interval it was taken over. With counters,
a query could ask for the rate over any window. Here, a 10-second spike
recorded at a 10-second interval is exact, and the same spike at a 60-second
interval is a sixth as tall. Rollups then average those.

**Names are flat, and a label is what varies.** CPU modes are separate
metrics (`sys_cpu_user_pct`, `sys_cpu_iowait_pct`), not a `mode` label,
because `sys_cpu_pct{cpu="3",mode="user"}` needs two labels to name one line,
and a list of names that each mean one thing is easier to read than a list
of one name with its modes. Every metric here differs from its siblings by
one label at most: `cpu`, `dev`, `iface`, `mount`, `proc`, `comm`, `user`,
or `unit`.

**A label beside that one says what kind of line it is, and never which.**
`pid`, `comm`, and `user` on a process, and `kind` on a unit, are there to
be filtered by. Equality is all that an element, and the metrics plane's
own range route, can ask of a label.

**`proc` names a process in one label.** `proc="postgres[1234]"` is what an
element selects. `pid`, `comm`, and `user` are on the same series so that a
query can select many at once.

## 2. "Every process" decides the tiers

The obvious design is a set of series per pid. On this project's
development host, a quiet minute ends about 800 processes. Most live for
milliseconds. A series costs a catalog entry of a few hundred bytes whether
it holds one point or a million, and each process would want fifteen. That
is more than ten million dead series a day, to record processes that were
never alive at a moment anyone sampled.

So a process is recorded in the tier that suits how long it lives:

| tier | what | cardinality |
|---|---|---|
| `unit_*` | series for one unit | services, scopes, and slices with something in them |
| `proc_*` | series for one process | processes that live past `--min-age` (30 s) |
| `procgroup_*` | totals by command name | distinct command names |
| `procuser_*` | totals by user | users |
| accounting record | one log entry per process that ended | none: it is a row, not a series |

The totals are what make the accounting complete. They include:

- every live process, whatever its age;
- everything a process used since it started, if it started within the
  interval;
- everything a process used between its last sample and its exit, from its
  exit record.

So a compiler that ran for 800 ms between two sweeps appears in
`procgroup_cpu_pct{comm="rustc"}` with all of its CPU, and in the log with
its full record, and has no series. In the verification run for this
document, a shell that burned CPU for 30 seconds had 29.88 CPU-seconds in
the group tier and 29.9 in its exit record, from independent sources: one
from sampling `/proc`, one from the kernel at exit.

Kernel threads are always in the totals, grouped by kind (`kworker/3:1-events`
counts under `kworker`), and get series of their own only on request.

## Units

`postgres[1234]` is a new series after every restart, because it is a new
process. What someone puts on a dashboard is `postgresql.service`.

### The kernel's accounts, not a sum of processes

A unit is a control group, and the kernel keeps accounts for each one: CPU
used, memory charged, bytes read and written, time stalled. They hold
everything that ever ran in the group. A sum over the processes a sweep
finds alive holds only what was alive at a sweep, and the rest has to be
recovered from exit records. For units the recovery is not needed: the
figure was never lost.

The accounts are also hierarchical, so a slice is read and not added up.
`system.slice` and `user.slice` are the two halves of the host, from two
files.

That is also why a list of units by size begins with what holds units.
On the development host the first five rows of `unit_memory_bytes` were
`user.slice`, `user-1000.slice`, `user@1000.service`, and two slices of
that user's, before the first application. The figures are right, and the
list says the same thing five times.

So every unit series has a `kind`: `service`, `scope`, `slice`, or
`manager`. A user's manager, `user@1000.service`, is a service by its name
and holds every unit the user runs, as a slice does, and no pattern over
names tells it from `getty@tty1.service`. It is a kind of its own, so that
`kind="service"` means a service that is only itself. The viewer leaves
out the same two kinds, by the same rule.

One account is not always kept. A group's I/O is counted only where the
I/O controller is enabled, and systemd does not enable it for a user's
manager. Every user unit on the development host, which is every container
on it, had no I/O account. There, I/O is added up from the unit's processes
at each sweep. It is the one figure in this tier that can miss a process
that did not live to a sweep, and the README says so.

Where a filesystem is on a device built on another (an encrypted or a
logical volume), the kernel charges its I/O to both. The device at the
bottom is counted.

### Naming

A label value has to name one line, and keep naming it.

**A unit of a user's manager is prefixed with the user.** Both managers can
have a `dbus.service`. The rule does not depend on whether there is a
collision today, so a name never changes because another unit appeared.

**An instance is named for what it is an instance of.** A desktop starts
each application in a scope named for the application and then the launch:
`app-Hyprland-chromium-2e5cb917.scope`. Under that name each launch is a
new set of series, and the application never has a line. The instance is
removed from the name, and the instances are added together: three
terminals are one line.

This is the same decision as the totals by command name, for the same
reason, and it has the same cost: two instances cannot be told apart in
this tier. They can in `proc_*`.

How much of a name is taken for an instance depends on who chose the name.
A scope is named by a launcher, by a convention that ends in an instance,
so any trailing number or random token is removed. A service is named by
the author of its unit file, so only a suffix too long to be a version is
removed: `postgresql-16.service` keeps its number, and
`omarchy-browser-1790704088432165608.service` does not.

**A unit named for a container is reported as what runs the container.**
A container runtime names a container's health check for the container's
id: sixty-four hexadecimal digits that say nothing and are different after
every restart. The first version reported every container's health checks
together, as `transient.service`.

Which container an id belongs to is in the control group tree already. A
container that a unit runs has its own group inside the unit's, named for
the id: `caddy.service/libpod-payload-0dc2…`. So the tree is read for
those, once a tick, and a unit named for a known id is reported as the unit
that runs the container. Its figures are added to that unit's: a health
check is part of what a container costs. The runtime is not asked, so
nothing depends on which runtime it is or on being allowed to ask it.

Adding instances together is done on differences, not on counters. When
one of three terminals is closed, its counters go with it, and a sum of
counters would fall. Each group's difference from its own last reading is
taken first, and the differences are added. A group that was not there at
the last reading was made since, so all it has used belongs to this
interval.

## 3. "Nothing unaccounted for" decides the source

Sampling `/proc` sees a process only if it is alive at a sweep. There are
three ways to learn of the rest:

| source | sees | needs | chosen |
|---|---|---|---|
| BSD accounting, `acct(2)` | every exit | root, and a file the kernel appends to forever | no |
| taskstats, over netlink | every exit, with delay and I/O accounting | `CAP_NET_ADMIN` | **yes** |
| eBPF | anything | `CAP_BPF`, a toolchain, kernel-version care | no |

taskstats is the kernel's own successor to BSD accounting: the same record
and more, delivered as a message rather than appended to a file that has to
be rotated. The conversation is small enough that this crate speaks netlink
directly, with no dependency for it.

### Threads

The kernel reports per task, and a task is a thread. One exit record per
thread of a 40-thread server would be wrong forty times. The threads of a
process are added together, and one record comes out when the kernel flags
the last of them (`AGROUP`, taskstats version 12, Linux 5.19).

Two things about this were learned by running it rather than by reading:

- **The flags lie about signals.** When a process exits, the kernel ends its
  other threads with a signal of its own, and flags them as signaled. The
  last thread of a process that called `exit(0)` is therefore "killed by a
  signal" with an exit status of 0. The wait status decides how a process
  ended; the flag is ignored.
- **The last thread is not the leader.** Threads name themselves
  (`tokio-runtime-w`), and the last to exit is whichever was slowest. The
  record is named for the thread group's leader when its exit was seen.

Threads that ended before the collector started are in the process's own
`/proc` figures and not in any record the collector received. Where a
process was also sampled, the record takes whichever of the two saw more.

### What is heard waits in a queue that is empty

A listener reads records as the kernel sends them, and the collector
takes them at a sweep. Between the two is a queue, which has to hold a
fork storm and is otherwise empty.

It was first a channel of 262,144 places, which Rust allocates when it is
made: 115 MB for a collector on a quiet host. Then one of 16,384, which
was 5 MB, and lost 7,850 records in the two busiest minutes of a browser
being compiled, 28,000 processes ending in each.

Both were the same mistake, of paying for the bound. The queue is now one
that grows, with a count of what is in it, and refuses at 262,144. It
costs what is waiting.

The two were run side by side through 120,000 processes started and
ended in six seconds. The queue of 16,384 lost 103,794 exit records and
92,693 execs. The one that grows lost none.

What is taken from the queue is kept for a sweep or two, in maps of
what is known of each process, and a map keeps the size it grew to when
it is emptied. So does the cache of pages SQLite has written. An hour
after that test, and after a compaction, the collector was at 290 MB
where it had been at 125: 48 MB was two maps with a few hundred entries
in them, and 22 MB was pages. The maps now give back the room of a burst
when it is over, and SQLite is asked for its pages at every flush. The
same test then left the collector 40 MB above where it began, two minutes
after.

Keeping them has a price, which is paid once they are taken. A tick's
records are made into log entries and spans together and written
together, at about 9 KB each while that is done: the collector was at
1.35 GB for one tick, and at 300 MB a minute later. Writing a tick in
parts would bound that. It is not done: a host that ends 19,000
processes a second is not the host this is for, and what it is for is
that the records are there afterwards.

### When the kernel says no

Without `CAP_NET_ADMIN` the collector runs anyway. A process that was there
at one sweep and gone at the next gets a record marked `source: "sampled"`,
with the figures of the last sweep that saw it and a status of `unknown`.
Those figures are a floor. Processes shorter than a sweep are not seen. The
collector says so when it starts, and `timeless-acct check` says what to do.

With the kernel reporting, a process found gone waits one sweep for its
record, in case the message is still in flight. If none comes, it was lost
(the kernel drops records when the socket buffer is full, and says only
that it did), and the sampled record is written instead. Every process that
was ever sampled gets exactly one record, one way or the other.

## What a process was

The kernel's record of an exit has the command's name and its numbers. It
does not have the arguments, the executable, or the unit: `cc1plus`, and
not what was being compiled. Those can only be read from `/proc` while the
process is alive, and most processes are gone before a sweep comes round.

They are learned from the first of these that applies.

1. **A sweep**, if the process lived to one.
2. **The moment it called exec.** The kernel's process connector reports
   each exec as it happens. A thread reads what the process is running
   right then, and the description waits for the exit record it belongs
   to. As little is read as will do, because each read is time in which a
   process that lives for a millisecond can end.
3. **Its parent**, if it never called exec. It ran what its parent runs.

The third was not planned. The first version with exec events gave a
command line to 120 of 493 records. The rest had never called exec:
postgres starts a worker by forking, a shell runs a subshell by forking,
and on the development host nine processes in ten are made that way. The
kernel says so in the exit record itself (`AFORK`), and the record is
marked `forked` so that a reader knows whose description it is reading.

Two checks keep the third from making things up.

- **The kernel has to say the process never called exec.** A process that
  did, unheard, ran something else, and gets no description rather than
  its parent's.
- **The parent has to have the same name.** The parent on record at a
  process's exit is whoever it was last handed to. A child whose own
  parent ended first belongs to pid 1 by then, and is not described as
  systemd.

With all three, 1,832 of 1,841 records had a command line. The nine
without were kernel threads, and children whose parent had ended first.

Exec events are asked for without a filter. Newer kernels can be asked for
exec events alone, in a longer request that older kernels discard without
an error, leaving a listener that hears nothing and cannot tell. Every
kernel accepts the short request and sends every kind of event. Sorting
them here costs less than telling the kernels apart.

### What is not kept

The first process opened in the viewer had a secret in its command line,
as an argument. It is there in `ps` for anyone on the host. But `ps` shows
it while the process runs, to whoever is on the host then. A store keeps
it for ninety days, for whoever can read the store, and the planes are
somewhere else again.

So an argument that is plainly a secret is withheld when the command line
is read, before it is anywhere: the value of an argument named for a
secret, the argument after a flag named for one, the password in a URL.
The name of the argument is kept, so that a command line still says what
was run and how.

This is a list of names, and a secret with another name goes through. A
list that errs the other way, withholding what might be one, would take
out paths and hostnames and leave command lines that say nothing; and a
collector cannot know. It is said in the README, where someone deciding
what to send where will read it.

### What it was started as

A process can call exec more than once, and a shell does it as a matter of
course: given `sleep 11; false`, it runs `sleep` as a child and then
becomes `false`, in its own place. The process that ends is `false`, with
an exit status of 1, after eleven seconds.

The last thing a process ran is what its record is about: the name, the
exit status, and the peak memory are that program's. But `false` that took
eleven seconds explains nothing, and `sh -c 'sleep 11; false'` explains it.
So the first command line is kept beside the last, as `started_as`, when
they differ.

One first command line is not kept. systemd starts every process of every
unit through `systemd-executor`, which becomes the program. It would be
what every service was started as, and it says nothing of any of them.

## Traces

A process has a start, a duration, and a parent, which is what a span is.
What it does not have is a trace. The tree of processes has one root, pid
1, and a trace of everything since boot says nothing.

### A trace is a job

A job is a process group. A shell makes one for each command it is given,
a pipeline is one, and systemd makes one for each run of a service.
`cargo build` and every compiler it starts are one trace. The shell it was
typed into is in another.

A span's parent is the process that started it, if that process is in the
same trace. The three commands of a pipeline are all children of the
shell, which is not, so a pipeline is a trace with three roots.

### Except for daemons

A job ends. A daemon's process group does not, and everything postgres
starts in a month would be one trace. So a process joins its group's trace
only if it started within an hour of the group. Past that, what the group
starts is the root of a trace of its own, and what that starts in turn is
part of it.

The hour is a guess at the longest job worth drawing, and it can be
changed. What it costs is that a build longer than an hour is cut: what it
starts after the first hour are traces of their own. The other rule
considered was to cut at a parent older than some age when the child
starts, which cuts every long build at that age rather than at an hour.

### Ids are decided at the start

A child ends before its parent. Its span is written first, and must
already carry the trace id its parent's span will carry when the parent
ends, minutes later. So a process's place is decided when it is first
heard of, from what is known then, and does not change.

And the ids are made, not drawn: a hash of the boot, the pid, and the
start. A collector that is restarted gives a running process the id it
gave it before, and the halves of a trace written before and after the
restart join.

### Everything heard of in a tick is laid out first

In one tick the collector hears of execs and of exits, in the order they
happened within each, and a child is heard of before its parent as often
as after. A subshell calls exec after the commands it ran did, and ends
after them too.

The first version placed each process as it came. A `sleep` run by a
subshell was placed before the subshell's own exec had been read, found no
parent, and was drawn as a root beside the tree it belonged in.

So a tick is taken in two passes. Everything heard of is laid out, and
then each is given its place, with all of the rest there to be found. A
parent is looked for in what the sweep tracks, in what called exec, in
what is ending in this same tick, and last in `/proc` itself: a subshell
that is still running has not called exec, has not ended, and has not
lived to a sweep, and is there.

### A service is a unit on a host

Metrics have a label for the host, and accounting records a field that
the logs plane keeps an index of. Spans had the host on their resource,
as OpenTelemetry has it, and it was no use there: pushed from two hosts,
the same unit was one service in the traces plane, which finds spans by
their service and their name and has nothing to ask a resource with.

So the host is in the service's name: `web-1/caddy.service`. It is there
in a local store as well, which holds one host and has no need of it,
because a store is laid out to be handed to the planes as it stands, and
a span that is one thing pushed and another handed over is two things.

The other way was an index on `host.name` in the plane's own table. It
keeps the names clean, and it is a change to every stack that is to
receive spans, made before the first span arrives: the indexes of a
traces table are chosen when it is created.

What reads a span for its unit reads `process.unit`, and does not take
the name apart.

### The same figures, to two readers

A span is made from the accounting record of the same process, not beside
it, so the two cannot disagree. They are kept twice because they are found
differently. A record is found by what happened: everything that died of
SIGSEGV. A span is found by where it happened: everything this build ran,
and in what order. Together they cost about 90 bytes a process.

Integers are sent to the traces plane as JSON numbers. OTLP writes a
64-bit integer as a string, and the plane, given one, stores a string:
`"process.pid": "173994"`. Nothing counted here comes near the 53 bits a
JSON number holds exactly, and as numbers they are stored as they are in a
local store.

## Accounting records are log entries

Their metadata uses the four keys the logs plane indexes by default:

| key | holds |
|---|---|
| `service` | the command name |
| `host` | the host |
| `path` | the executable, when the process lived long enough to be sampled |
| `status` | the exit code, or the signal's name |

So "every exit of postgres" and "everything that died of SIGSEGV" are index
lookups. Everything else (CPU, memory high-water mark, I/O, faults, delays,
the command line) is typed metadata on the entry.

### The level is a judgement about the host

A canvas host element turns red when the host logged an error in the last
minute, and amber for a warning. So the level of an exit record decides the
colour of the host, and is chosen for that:

| ending | level | why |
|---|---|---|
| exit 0 | info | |
| exit non-zero | notice | it is how `grep` says "no match" |
| SIGTERM, SIGINT, SIGHUP, SIGPIPE, … | notice | someone asked it to stop |
| SIGKILL | warning | someone insisted, or the out-of-memory killer did |
| SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGABRT, SIGSYS | error | a program on this host is defective |

## Values are stored at the precision they were measured to

Thousandths, or whole units from a thousand up. A rate is a difference of
integer counters divided by a measured interval; its digits past the first
few describe the jitter of the interval. `13.99709459881323` and `13.997`
are the same measurement, and the second is a third the size on the wire
and what the store's float compression is built for.

## One encoder, two sinks

Metrics leave as Prometheus exposition text and records as NDJSON. The
embedded engine's ingest column and the planes' HTTP routes accept exactly
those, so both sinks share one encoder and cannot drift. The binary batch
format would be faster to ingest; at a few thousand samples a tick, it is
not the cost that matters.

## The local store is laid out as the planes lay out theirs

One database per signal, the metrics table named `metric_samples` and the
logs table `logs`, the same indexed keys, the same owner lease file.

A Timeless signal server takes a lease on a whole database file, so one
file holding both signals could only ever be served by one server. With the
planes' own layout, a directory written by the collector can be handed to
`timeless-metrics-api` and `timeless-logs-api` as it stands, and the lease
stops a server and a collector from owning it at the same time. Both
directions were verified.

The virtual tables are passive, so the collector does for its store what
the servers do for theirs: it flushes every minute and compacts every hour.

### Why every hour

The servers compact every five minutes, and the collector's first version
did the same. It cost 10.6 bytes a sample on disk, for data that compresses
to 0.4.

A chunk has a fixed cost that does not depend on what is in it. Measured on
this collector's own data, for a series whose value never changed, sampled
at perfectly regular times:

| | bytes |
|---|---:|
| the values, at the codec's floor | 28 |
| the timestamps | 55 |
| the row and its index entry in SQLite | about 72 |

Four fifths of all chunks were exactly that: 155 bytes to record that
nothing happened. Re-storing one hour of samples, 1.4 million of them, with
only the chunk size changed:

| samples in a chunk | bytes a sample, on disk |
|---:|---:|
| 30 | 6.28 |
| 60 | 3.01 |
| 120 | 1.69 |
| 360 | 1.09 |

The compression was never the problem. The number of chunks was.

The first three rows fit 0.4 bytes a sample and 155 bytes a chunk, which
predicts 0.83 for the fourth. It measured 1.09: the timestamps of a chunk
took 119 bytes at 360 samples and 53 at 120, so a chunk's cost is not quite
fixed. Why has not been looked into. Twenty minutes of samples taken every
second, at 1,200 to a chunk, stored at 0.21.

The number of chunks is decided by how often the store is compacted. Each
compaction turns whatever a series has gathered since the last one into one
compressed chunk. After that, the engine merges a series' chunks only once
they hold 16,384 samples between them: 45 hours of a series sampled every
ten seconds, and never for the series of a process that lives for less.
Most do. So for most of what this collector stores, the size of a chunk at
its first compaction is the size it keeps.

Every hour is 360 samples to a chunk. What it costs is that the samples of
the last hour sit uncompressed until then, in the small chunks each flush
wrote: under 30 bytes a sample, for an hour's worth. Flushing is what makes
samples survive a crash, so it stays at a minute.

### Compaction is due at the start as well

Compaction was due an hour from when the collector started. A collector
restarted every half hour never reached it, and its store stayed as the
flushes left it: 79,835 chunks of six samples each, after twenty minutes
on the development host. What the last run flushed and did not live to
compact is now compacted when the next one starts.

### What compaction frees is given back

`PRAGMA incremental_vacuum` answers with a row for each page it frees, and
frees a page for each row that is asked for. Run as a statement that
answers nothing, as a maintenance command would be, it frees one page and
reports no error. The store was compacted and the file did not shrink:
3,817 of its 4,597 pages were free. Asked for every row, the same file
went from 18.8 MB to 5.1.

### And so is what it was written through, and what it was done in

A maintenance pass rewrites much of a store, and all of it goes through
the write-ahead log. SQLite checkpoints a log and then writes over it from
the start; it does not make the file smaller. An hour and a half after it
began, the development host's store was 47 MB of databases and 95 MB of
logs, each log about 30 MB, with between 193 and 1,310 pages in use.
Checkpoints had kept up. The files were the size of the largest pass.

The same is true of memory. Compacting that store took the collector from
15 MB to 143, and glibc kept what was freed for the next allocation of
that size, which is an hour away. The collector stayed at 143 MB.

After a pass, each log is checkpointed and cut to nothing, and what the
allocator holds free is given back to the kernel: 143 MB became 59. A log
is also limited to 8 MB of what it grew to, where the planes allow 64.

Memory is given back after every flush as well. While a C build ended
62,000 processes in twelve minutes, the collector went from 124 MB to 367
in one half minute and stayed there; asked to give back what it held
free, it was 189.

What is left is the engine's own. It holds every series of the store in
memory, at about 1.4 KB each, and an entry of about 225 bytes for each
chunk: measured on one set of 30,000 series stored as 57,000 chunks, and
as 150,000, 450,000, and 900,000. Every flush writes a chunk for each
series that has samples, which was 1.2 MB a minute on the development
host, until the next compaction merges them. A series that has ended is
left with two or three, and costs about 1.9 KB. See "What is not here
yet".

### One thing differs on purpose

`PRAGMA auto_vacuum` can only be chosen while
a database is empty, and switching to WAL writes the header that ends that.
The collector sets it first. Set second, it is silently ignored.

## Watching

The viewer is what the canvas is for, in the place a collector already is:
a terminal on the host.

### Two sources, one reading

Now is read from the kernel, and every other moment from the store. Both
are turned into what is on the screen by the same code, from the same
metric names. The source of a moment is something that is asked for the
series of a metric, and answers from a reading just taken or from the
store at a time.

The other way was to read now from the store as well, which is one source
and no seam. But the store is a minute behind. The collector holds its
samples in memory until it flushes, and another process cannot see them.
Flushing every ten seconds to narrow the gap would have every series write
a chunk of one sample, six times a minute: the cost this document spends a
section getting rid of. So now is sampled, by the collectors themselves,
run a second time in the viewer. It costs a sweep every two seconds while
someone is watching, and nothing when no one is.

The seam is where the two meet. Going back from now lands on the last
moment the store holds, not on ten seconds ago, because ten seconds ago is
not in the store yet. And the viewer reads the kernel as whoever runs it,
which is seldom with the collector's privileges.

### The store is read and never written

The viewer opens the store read-only and takes no lease. The collector
goes on filling it, and what it has flushed since the last look is there
at the next.

### The timeline

Going back through time with nothing to go by is going back blind. Across
the top of the screen is how busy the host was over a stretch of time,
with the moment looked at marked under it, and what went wrong marked
where it happened.

What went wrong is what the accounting records call an error or a
warning: a process that ended by a fault, or was killed. These are the
records that turn a host red or amber on a canvas, for the same reason.

A stretch of hours is read from the samples. A longer one is read from
what the store keeps of the samples after they are gone, at five minutes
and at an hour. Those are made when the store is compacted, once an hour,
so the last hour is not among them; it is read from the samples, and put
after them. The first version did not, and a timeline of six hours ended
an hour ago.

The timeline is drawn to the highest figure in the stretch, which the
frame says. A host is seldom near all of its CPUs, and drawn to a hundred
percent its busiest hour would be a flat line.

### What is looked for is looked for in the store

The first version read the last two hundred exits and showed those that
matched. On the development host two hundred exits is ten seconds. A
process that had died of a fault two minutes before was not among them,
and looking for `SIGSEGV` found nothing.

What is wanted is now decided as the store is read, and the reading goes
on until there are enough or the stretch is at an end. And when something
is looked for, the stretch is as long as the timeline's, and not the
quarter of an hour that is enough for looking at what just happened.

### And read a page at a time

That was written on a quiet host. Over an hour in which a quarter of a
million processes ended, looking for `rustc` stopped the screen for four
seconds among the exits and twenty-five among the jobs, and left the
viewer at 2.85 GB.

The logs engine hands over every record it is asked for before the first
can be looked at, at about ten kilobytes each: to count that hour took
the same four seconds and the same memory as to read it
([timeless-libsql#77](https://github.com/awksedgreep/timeless-libsql/issues/77)).
Three things it does do are what the viewer now asks for.

It stops at a number it is given, if the stretch is given with both its
ends in it; with `ts < ?` for an end it reads everything
([#89](https://github.com/awksedgreep/timeless-libsql/issues/89)). So the exits
with nothing looked for are one question, of the last two hundred, and
an hour costs what a minute does.

It looks for a text in what a record says, itself, before handing
anything over. A record says the command's name and how it ended, so
those are found over the whole stretch at once: `rustc` in 50 ms.

And what a record does not say (what it ran in full, its unit, its user)
is read for in pages of 20,000 records, the latest first, each page up to
where the one before ended. A page is a fifth of a gigabyte while it is
read and is given back after. The reading is done between the keys: the
screen shows what has been found and how far back the reading has got,
and a key is answered at once. It ends when a screen's worth is found
with nothing later unread, which for anything that happens often is the
first page.

Slices of time were tried first, and were the wrong unit twice. How many
records a minute holds is not known until it is read, and a slice of a
quiet hour that ran into a build was a gigabyte. And the engine reads
whole blocks, some of which span half an hour: a one-second slice under
such a block cost what a minute did, and the hour took a hundred seconds.

The spans engine hands rows over one at a time, and an hour of them is a
third of a second. So the jobs are read in one pass: which there are,
when each began, and whether anything that ran in it is what is looked
for. What took twenty-five seconds was asking for each job in turn to see
whether its first command matched, which also meant a build was not found
by its compiler.

| over that hour | before | now |
|---|---|---|
| the exits, nothing looked for | 473 MB | 0.2 s, 257 MB while read |
| `rustc`, among the exits | 4.1 s | 0.5 s |
| a unit's name, among the exits | 4 s | 0.9 s |
| a text that is not there | 4 s, the screen stopped | 7 s, the screen answering |
| `rustc`, among the jobs | 25.3 s, and none found | 1.8 s, the builds |
| the viewer afterwards | 2,849 MB | about 160 MB |

### Jobs that have not ended

A span is written when its process ends, so the store knows a job when it
is over. What is running is read from the processes that are there, by
the rule the store's jobs are made by: a process group of more than one
process, no older than a job gets. They are the same jobs, before and
after.

This is in the viewer and not in the collector. The collector could write
what is running at each sweep, but a job that runs for ten minutes would
be written sixty times, to be read once if at all.

### Into a unit

A process's series carry the unit it is in, as a label. That is what lets
a unit be opened into its processes at a moment in the past, when there is
no `/proc` to ask which process was where. It is also one more way to
select many processes by one label, which is all a canvas element has.

### What is asked of the store

A moment is the last sample of each series in the three samples before
it. A process that had ended by then has no sample in those, and is not
on the screen: no list of what was alive is kept, because the samples
are one.

How far apart samples are is what the collector was told when it was
started, and a store does not say what it was told. The viewer measures
it, from the samples before the moment looked at: the gap that half of
them are no further apart than, so that a collector that was stopped for
an hour is a gap and not the spacing. A step in time is one sample of
whichever is sampled more often, the system or the processes, and what
is there at a moment is looked for over three of whichever is sampled
less.

It was thirty seconds and a step of ten, which are those of the default.
A store sampled every forty seconds had nothing on the screen at most
moments, and one sampled every second was stepped through ten at a time.
On the same host over the same four minutes, the browser's highest CPU
was 134% in the store sampled every second and 32% in the one sampled
every forty.

Holding an arrow down goes through time without reading each moment passed
on the way. Every key that is waiting is taken before anything is read.

## Ticks land on round times

A reading is taken at each multiple of the interval on the wall clock, and
stamped with that time. Two hosts sample at the same moments, and a graph
bucket never splits an interval. Rates use the monotonic clock for the
length of the interval. A sweep that overruns, or a host that sleeps, skips
the ticks it missed rather than running them late.

## No async runtime

A collector wakes on a timer, reads files, and writes a batch. There are
three threads: the loop, and one blocked on each netlink socket. HTTP is
blocking, with a timeout shorter than the interval.

## What is not here yet

Ordered by how much each would add.

1. **A bound on memory.** The engine holds every series of a store in
   memory, and a process is fifteen new ones. Two things would help, and
   neither is done. A process could be given series of its own later than
   thirty seconds into its life: 41% of those that ended had lived for
   less than five minutes. And the engine could keep in memory only the
   series being written, which is the engine's to do.

   Half of all series never hold a value but zero, and they are kept on
   purpose. A series that says nothing happened is an answer, and one
   that is absent is a question; its samples compress to almost nothing.
   What it costs is what any series costs, in the index and for each
   chunk.
2. **The end of a series.** Prometheus marks a series as over with a value
   that is not a number, and a reader stops at it. Here a reader is told
   how far back to look. It takes the collector and the plane both: the
   collector drops what is not finite before it is stored, and the metrics
   plane was found to know the marker on its MetricsQL route and not on
   its PromQL one.
3. **Containers that no unit runs.** One started by hand is in a scope
   named for its id and nothing else. Its name is known to the runtime
   alone, so naming it means asking: reading podman's or docker's state,
   which differs by runtime and by version, and takes being allowed to.
4. **Units where systemd does not name them**: Docker with its own control
   group driver, Kubernetes. The accounts are the same files; what differs
   is how a group's path is turned into a name.
5. **A trace's resources, while it runs.** A span is written when its
   process ends, so a build is drawn when it is over. The processes of a
   job that is still running are known, and could be listed.
6. **Memory by proportional set size**, from `smaps_rollup`. Resident size
   counts shared pages once per process that maps them, so a forking server
   looks larger than it is. It costs a page-table walk per process per
   sweep, so it would be opt-in. For a unit, `unit_memory_bytes` already
   counts each page once.
7. **The binary batch format** for the embedded sink, if ingest cost ever
   shows up in a profile.
