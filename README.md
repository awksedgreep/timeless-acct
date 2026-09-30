# timeless-acct

**System and process accounting history for Linux, in
[timeless-libsql](https://github.com/awksedgreep/timeless-libsql).**

`timeless-acct` records five things:

- what **sar** records: CPU, memory, swap, paging, disks, interfaces, TCP,
  pressure, filesystems;
- a set of series for every **unit**: each systemd service, each container,
  each desktop application, under a name that outlives a restart;
- a set of series for every **process** that lives long enough to have one;
- an **accounting record for every process that ends**, however briefly it
  lived, with what it was running;
- a **trace for every job**: each build, each pipeline, each run of a
  service, as the tree of processes it was.

It stores them in timeless-libsql, so they can be put on a
[Timeless canvas](https://github.com/awksedgreep/timeless_canvas) and the
timeline dragged back: what was this host doing, and which process was
doing it, at 03:12 last Tuesday.

And it can be watched where it is, in a terminal, with the same timeline:

```text
$ timeless-acct watch
┌ timeless-acct ────────────────────────────────────────── ◀ 2026-09-29 16:50:40  10m23s ago ┐
│load 2.01 2.61 2.20   cpu 2.3% (user 1.6 sys 0.5 wait 0.8)   mem 14.8 GiB of 93.8 GiB       │
│disk ↓0 B/s ↑158 KiB/s   net ↓333 KiB/s ↑26.4 KiB/s   tasks 1677   stalled: cpu 0.1% …      │
└────────────────────────────────────────────────────────────────────────────────────────────┘
 1 Units  2 Processes  3 Jobs  4 Exits    by cpu
UNIT                                            CPU%    MEMORY  PROCS  TASKS  READ/s   WRITE/s
mark/app-Hyprland-chromium.scope             23.0   3.9 GiB     16    305     0 B  28.4 KiB
mark/app-Hyprland-xdg-terminal-exec.scope    10.3   6.2 GiB     23    242     0 B   3.6 KiB
mark/wayland-wm@hyprland.desktop.service      5.4   2.8 GiB      9     63     0 B       0 B
mark/timeless-stack.service                   1.5   2.4 GiB     11    230     0 B       0 B
┌ mark/app-Hyprland-chromium.scope cpu, the 10m00s before ──────────────────── peak 26.7% ┐
│                                                                                      ██   ▃│
│                                                                                      ████▆█│
└────────────────────────────────────────────────────────────────────────────────────────────┘
 ←→ 10s ,. 1m <> 10m [] 1h l live tab view ↑↓ row s sort a slices / only ? help q quit
```

```text
$ timeless-acct top --at "2026-09-29 14:45:45"
2026-09-29 14:45:45  (182 processes)
load 2.29 2.16 1.66   cpu 6.1%   mem 13.1 GiB of 93.8 GiB

     PID USER         CPU%       RSS  THR     READ/s    WRITE/s      TIME  COMMAND
   66858 mark         99.4   3.9 MiB    1        0 B        0 B     22.4s  sh
   11564 mark         15.4   704 MiB   37        0 B        0 B     1m55s  chromium
   10102 mark          3.4   248 MiB   36        0 B        0 B     4m39s  chromium

$ timeless-acct exits --status SIGSEGV
ENDED                    PID USER       STATUS      ELAPSED       CPU  PEAK RSS  COMMAND
2026-09-29 14:45:22    66900 mark       SIGSEGV       394ms       1ms   2.3 MiB  sleep
```

That `sleep` lived for 394 milliseconds. No sampler saw it; the kernel
reported it. These lived for one:

```text
ENDED                    PID USER       STATUS      ELAPSED       CPU  PEAK RSS  COMMAND
2026-09-29 16:01:21   131870 mark       3               1ms       2ms   3.9 MiB  sh -c exit 3 marker-sh
2026-09-29 16:01:21   131869 mark       2               1ms       0ms   2.5 MiB  ls /nonexistent-marker-ls
```

And this is one compile, as the kernel saw it:

```text
$ timeless-acct trees --comm cc1
2026-09-29 16:32:21  10 processes over 234ms, cpu 31ms, 2 failed  in mark/app-Hyprland-xdg-terminal-exec.scope  (trace 233db4a69584b3646c1da02a132e7186)
  /bin/sh ./build.sh  234ms, cpu 2ms, 4.0 MiB
  ├─ gcc -O2 -o hello hello.c  27ms, cpu 2ms, 3.4 MiB
  │  ├─ /usr/lib/gcc/x86_64-pc-linux-gnu/16/cc1 -quiet hello.c -quiet -dumpbase…  12ms, cpu 12ms, 36.6 MiB
  │  ├─ as --64 -o /tmp/ccTcxJCk.o /tmp/ccD1Vbk7.s  2ms, cpu 2ms, 4.7 MiB
  │  └─ /usr/lib/gcc/x86_64-pc-linux-gnu/16/collect2 -plugin /usr/lib/gcc/x86_6…  10ms, cpu 1ms, 2.7 MiB
  │     └─ /usr/bin/ld -plugin /usr/lib/gcc/x86_64-pc-linux-gnu/16/liblto_plugin.s…  9ms, cpu 9ms, 9.2 MiB
  ├─ ./hello  0ms, cpu 1ms, 1.5 MiB
  ├─ ls /nonexistent-dir  202ms, cpu 1ms, 2.4 MiB  [exited 2]
  │  └─ sleep 0.2  201ms, cpu 1ms, 2.3 MiB
  └─ grep -c nothing-here /etc/hostname  1ms, cpu 0ms, 2.5 MiB  [exited 1]
```

## Status

Version 0.2.1. Collection, both sinks, the viewer, and the three query
commands work and
are tested against a live kernel, on a host run by systemd with the unified
control group hierarchy. [What is not here yet](#what-is-not-here-yet)
is listed at the end, and [DESIGN.md](DESIGN.md) explains the decisions.

Linux only, x86-64 and aarch64. Exit accounting needs Linux 5.19 or later to
account a multi-threaded process as one process.

## Quick start

```sh
cargo build --release
target/release/timeless-acct check
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

### To the canvas

The canvas reads from the Timeless planes. Point the collector at them:

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

#### Reading with PromQL

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
The viewer and `timeless-acct top` use thirty seconds.

A host element turns red when a process on the host dies of a fault, and
amber when one is killed; see
[the level of a record](DESIGN.md#the-level-is-a-judgement-about-the-host).

### To a store of its own

```sh
timeless-acct run --sink embedded --data-dir /var/lib/timeless-acct
```

No server, no network. Watch it with `watch`, read it back with `top`,
`exits`, and `trees`, or with SQL through the timeless extension. The directory is laid out as the
planes lay out theirs, so it can be served to a canvas later:

```sh
timeless-metrics-api libtimeless_ext.so /var/lib/timeless-acct/metrics.db
TIMELESS_LOGS_TIMESTAMP_UNIT=us \
  timeless-logs-api libtimeless_ext.so /var/lib/timeless-acct/logs.db
timeless-traces-api libtimeless_ext.so /var/lib/timeless-acct/traces.db
```

One owner at a time: a server and a collector cannot both hold a store, and
whichever comes second is refused.

## Privileges

Without any, the collector still runs. Each capability adds something:

| capability | adds |
|---|---|
| none | system statistics; units; CPU and memory of every process; I/O and open files of the user's own |
| `CAP_NET_ADMIN` | exit records from the kernel: every process, however short-lived, and what each was running |
| `CAP_SYS_PTRACE` and `CAP_DAC_READ_SEARCH` | I/O, open files, and executable path of other users' processes. It takes both |

[dist/timeless-acct.service](dist/timeless-acct.service) runs the collector
as an unprivileged user holding exactly those three. For a binary run by
hand:

```sh
sudo setcap cap_net_admin,cap_sys_ptrace,cap_dac_read_search+ep target/release/timeless-acct
```

Exit accounting is offered only to the initial user and PID namespaces: not
inside a container, and not under `PrivateUsers=`.

Delay accounting (time spent waiting for a CPU, for block I/O, for memory)
is compiled into most kernels and off by default. Turn it on with
`sysctl kernel.task_delayacct=1`.

## What is recorded

Every metric is a gauge, with the rate already taken: the canvas draws the
last value in a bucket, and sar has always recorded rates. Every series
carries `host`.

### The system: `sys_*`

| sar | metrics | label |
|---|---|---|
| `-u`, `-P ALL` | `sys_cpu_{user,nice,system,iowait,irq,softirq,steal,guest,idle,busy}_pct` | `cpu` = `all`, `0`, `1`, … |
| `-q` | `sys_load{1,5,15}`, `sys_procs_running`, `sys_procs_blocked`, `sys_tasks` | |
| `-q CPU,IO,MEM` | `sys_pressure_{cpu,memory,io}_{some,full}_{avg10,pct}` | |
| `-w` | `sys_forks_per_sec`, `sys_context_switches_per_sec` | |
| `-I SUM` | `sys_interrupts_per_sec`, `sys_softirqs_per_sec` | |
| `-r` | `sys_mem_{total,available,used,free,buffers,cached,active,inactive,anon,shmem,slab,kernel_stack,page_tables,dirty,writeback,committed}_bytes`, `sys_mem_used_pct`, `sys_mem_committed_pct` | |
| `-S` | `sys_swap_{total,used}_bytes`, `sys_swap_used_pct` | |
| `-W` | `sys_swap_{in,out}_pages_per_sec` | |
| `-B` | `sys_page_{in,out}_bytes_per_sec`, `sys_page_faults_per_sec`, `sys_major_faults_per_sec`, `sys_pages_{freed,scanned,reclaimed}_per_sec`, `sys_oom_kills_per_sec` | |
| `-H` | `sys_hugepages_{total,free}` | |
| `-b` | `sys_io_{reads,writes}_per_sec`, `sys_io_{read,write}_bytes_per_sec` | |
| `-d` | `sys_disk_{reads,writes}_per_sec`, `sys_disk_{read,write}_bytes_per_sec`, `sys_disk_util_pct`, `sys_disk_queue_depth`, `sys_disk_await_ms` | `dev` |
| `-F` | `sys_fs_{size,used,available}_bytes`, `sys_fs_used_pct`, `sys_fs_inodes_used_pct` | `mount` |
| `-n DEV`, `EDEV` | `sys_net_{rx,tx}_{bytes,packets,errors,dropped}_per_sec` | `iface` |
| `-n SOCK` | `sys_sockets`, `sys_tcp_sockets`, `sys_tcp_sockets_orphan`, `sys_tcp_sockets_time_wait`, `sys_udp_sockets` | |
| `-n TCP`, `ETCP` | `sys_tcp_connections`, `sys_tcp_{active,passive}_opens_per_sec`, `sys_tcp_segments_{in,out}_per_sec`, `sys_tcp_retransmits_per_sec`, `sys_tcp_attempt_fails_per_sec`, `sys_tcp_resets_per_sec` | |
| `-n UDP` | `sys_udp_datagrams_{in,out}_per_sec`, `sys_udp_errors_in_per_sec` | |
| `-v` | `sys_file_handles`, `sys_file_handles_pct`, `sys_inodes`, `sys_dentries_unused` | |
| `-m TEMP` | `sys_temp_celsius` | `sensor` = `coretemp/Package id 0`, `nvme:nvme1/Composite` |
| `-m FAN` | `sys_fan_rpm` | `sensor` |
| `-m CPU` | `sys_cpu_mhz`, `sys_cpu_mhz_max`: the mean over the CPUs, and the fastest | |
| | `sys_power_watts`: what each power domain draws | `domain` = `package-0`, `package-0/core` |
| | `sys_uptime_seconds` | |

A sensor is named for its chip and for what the chip calls it. Where a
host has two of a chip, as it has of a drive, each is named for the device
it is on as well. Power is read from the running average power limit
interface, which only root may read: it takes `CAP_DAC_READ_SEARCH`.

`sys_cpu_*_pct` is a share of all CPUs, 0 to 100, as sar reports it.
Partitions, loop devices, and container `veth` interfaces are not reported.
A filesystem mounted at several paths (btrfs subvolumes) is reported once.

### Processes: `proc_*`

For each process older than `--min-age` (30 seconds). Labels: `proc`
(`postgres[1234]`), `pid`, `comm`, `user`, and `unit` if it is in one.
`proc` names one process. Each of the others selects many: every process
of a command, of a user, or of a unit.

| metric | is |
|---|---|
| `proc_cpu_pct`, `proc_cpu_user_pct`, `proc_cpu_system_pct` | share of **one** CPU, as top reports it: a process with four busy threads is at 400 |
| `proc_cpu_seconds` | CPU used in its life so far |
| `proc_cpu_wait_pct` | share of time runnable and waiting for a CPU |
| `proc_io_wait_pct` | share of time waiting on block I/O; needs delay accounting |
| `proc_rss_bytes`, `proc_vsize_bytes`, `proc_swap_bytes` | memory |
| `proc_threads`, `proc_fds` | threads and open files |
| `proc_io_read_bytes_per_sec`, `proc_io_write_bytes_per_sec` | bytes that reached storage; page-cache hits and pipes are not in it |
| `proc_minor_faults_per_sec`, `proc_major_faults_per_sec` | page faults |
| `proc_context_switches_per_sec` | voluntary and involuntary together |

### Units: `unit_*`

For each systemd service, scope, and slice that has anything running in
it. Labels: `unit`, and `kind`.

| `kind` | is |
|---|---|
| `service`, `scope` | a unit that is only itself |
| `slice` | a slice: the sum of the units in it |
| `manager` | a user's manager, `user@1000.service`: the sum of every unit that user runs |

These are the kernel's own accounts of each control group, not sums over
processes: they hold everything that ran in the unit, including what
started and ended between two readings.

| metric | is |
|---|---|
| `unit_cpu_pct`, `unit_cpu_user_pct`, `unit_cpu_system_pct` | share of one CPU |
| `unit_cpu_throttled_pct` | share of time held back by a CPU limit; only for a unit that has one |
| `unit_memory_bytes` | everything charged to it: its processes' memory, and the page cache of the files they use |
| `unit_memory_anon_bytes`, `unit_memory_file_bytes` | those two parts |
| `unit_swap_bytes` | |
| `unit_processes`, `unit_tasks` | processes, and threads |
| `unit_io_read_bytes_per_sec`, `unit_io_write_bytes_per_sec` | bytes that reached storage |
| `unit_pressure_cpu_pct`, `unit_pressure_memory_pct`, `unit_pressure_io_pct` | share of time something in it was stalled waiting for that |
| `unit_oom_kills_per_sec` | |

How a unit is named:

| control group | reported as |
|---|---|
| `postgresql.service` | `postgresql.service` |
| a unit of a user's manager | `mark/caddy.service` |
| `app-Hyprland-chromium-2e5cb917.scope` | `mark/app-Hyprland-chromium.scope` |
| `session-2.scope`, `run-p5134-i78156.scope` | `session.scope`, `run.scope` |
| `systemd-coredump@2-12289-0.service` | `systemd-coredump@.service` |
| named for the id of a container that a unit runs | that unit: `mark/caddy.service` |
| named for an id and nothing else | `transient.service` |

A desktop starts every application in a scope of its own, named for the
application and then the instance. Under their full names each launch
would be a new set of series; under the application's, three terminals are
one line, added together. A service keeps the name its author gave it:
`postgresql-16.service` keeps its number.

A container's health check runs in a unit of its own, named for the
container's id. It is reported as part of the unit that runs the container:
it is part of what the container costs. Which unit that is, is read from
the control group tree, where the container's own group sits inside the
unit's; the container runtime is not asked.

A slice is reported beside the units in it, so `system.slice` and
`user.slice` are the two halves of the host. Ranked by size, the slices
and the managers come first, above what they are the sum of; `kind` is
for leaving them out.

The kernel keeps a unit's I/O only where the I/O controller is on, which
for a user's units it is not by default. There, I/O is added up from the
unit's processes at each sweep: all of it, but for processes that did not
live to one.

### Totals: `procgroup_*` and `procuser_*`

Over **every** process, including those too young or too brief for series
of their own, by command name (`comm`) and by user (`user`).

| metric | is |
|---|---|
| `procgroup_processes`, `procgroup_threads`, `procgroup_rss_bytes` | now |
| `procgroup_cpu_pct` | share of one CPU over the interval, including processes that ended in it |
| `procgroup_io_{read,write}_bytes_per_sec` | |
| `procuser_processes`, `procuser_rss_bytes`, `procuser_cpu_pct` | the same, by user |

### The collector: `acct_*`

`acct_processes`, `acct_processes_reported`, `acct_sweep_seconds`,
`acct_exits`, `acct_exits_lost`, `acct_execs`, `acct_execs_missed`,
`acct_execs_lost`.

A rising `acct_exits_lost` means processes are ending faster than their
records can be read. Up to 262,144 wait between two sweeps, which at the
default interval is 26,000 tasks ending a second; a browser being compiled
on 22 CPUs ended 470 processes a second at its busiest.
`acct_execs_missed` counts processes that were gone
before they could be described: their records have a name and no
arguments.

### Accounting records

One log entry per process that ended. The message reads:

```text
cc1plus[4242] exited 1 after 2.5s, cpu 2.2s, peak rss 500 MiB
sleep[66900] killed by SIGSEGV (core dumped) after 394ms, cpu 1ms, peak rss 2.3 MiB
```

Indexed: `service` (the command name), `host`, `status` (exit code or
signal name), `path` (the executable). And as typed metadata:

| field | |
|---|---|
| `pid`, `ppid`, `uid`, `gid`, `user`, `nice`, `threads` | |
| `exit_code`, or `signal` and `core_dumped` | |
| `started`, `elapsed_seconds` | |
| `cpu_seconds`, `cpu_user_seconds`, `cpu_system_seconds`, `cpu_pct` | |
| `peak_rss_bytes`, `peak_vm_bytes`, `minor_faults`, `major_faults` | |
| `io_read_bytes`, `io_write_bytes`, `io_read_chars`, `io_write_chars` | |
| `context_switches_voluntary`, `context_switches_involuntary` | |
| `delay_{cpu,blkio,swapin,reclaim,thrashing}_seconds` | zero unless delay accounting is on |
| `cmdline`, `path`, `unit` | what it was running, and where |
| `started_as` | what it ran first, if that was something else: a shell runs the last command it is given in its own place |
| `forked` | it never called exec, and is described as its parent |
| `source` | `taskstats`: the kernel's record. `sampled`: the process was noticed gone, and the figures are those of the last sweep that saw it |

### Secrets

A command line is there for anyone on the host to read while its process
runs. In a store it is there for whoever can read the store, for as long
as the store is kept, and wherever it is sent. So what is plainly a secret
is withheld before anything is recorded:

| in a command line | is recorded as |
|---|---|
| `--password=hunter2`, `API_KEY=abc` | `--password=***`, `API_KEY=***` |
| `--token abc123`, `-setcookie XYZ` | `--token ***`, `-setcookie ***` |
| `postgres://app:hunter2@db/main` | `postgres://app:***@db/main` |

An argument is taken for a secret by its name: `password`, `passwd`,
`passphrase`, `secret`, `token`, `cookie`, `credential`, `apikey`. **A
secret that does not say it is one is recorded**, as `ps` shows it: a bare
positional argument, or a flag named for something else.

A local store is made readable by its owner alone.

What a process was running is learned at the first of these that
applies: from a sweep, if it lived to one; at the moment it called exec;
or, if it never did, from its parent, whose program it ran. Measured over a
minute of this host: 1,841 processes ended, 1,650 of them without ever
calling exec, and 1,832 records had a command line. The nine without were
kernel threads, and children whose parent had ended before them.

### Traces

One span per process that ended, of the same processes as the accounting
records and with the same figures. A record is found by what happened:
everything that died of SIGSEGV. A span is found by where it happened:
everything this build ran, and in what order.

| of a span | is |
|---|---|
| name | the command's name: `rustc` |
| service | the unit it ran in, on its host: `web-1/timeless-stack.service`; `web-1/-` if it ran in none |
| parent | the process that started it, if that is part of the same trace |
| status | `ok`, or `error` with `exited 2` or `killed by SIGSEGV` |
| attributes | `process.pid`, `process.parent_pid`, `process.owner`, `process.command_line`, `process.started_as`, `process.executable.path`, `process.exit.code`, `process.signal`, `process.unit`, `process.forked`, `process.threads`, `process.cpu_seconds`, `process.cpu_pct`, `process.peak_rss_bytes`, `process.io_read_bytes`, `process.io_write_bytes` |

So in anything that reads traces, the services are the units of each
host, and the operations of a service are the commands that ran in it.

The host is part of the service's name, and not only beside it, because
the traces plane tells spans apart by their service and their name and by
nothing else. The same unit on two hosts would be one service, with no way
to ask for either's. The unit by itself is `process.unit`, and the host
`host.name` on the span's resource.

**A trace is a job**: a process group. A shell makes one for each command
it is given, a pipeline is one, and systemd makes one for each run of a
service. The shell a build was typed into is not part of the build.

A daemon's process group never ends, so a process is part of its group's
trace only if it started within an hour of it (`--trace-max-age`). After
that, each thing the daemon starts is the root of a trace of its own: a
postgres worker is a trace of one process, and a health check is a trace
of two.

The kernel's own threads have no spans.

## Watching a local store

```sh
timeless-acct watch --data-dir /var/lib/timeless-acct
timeless-acct watch --at "2026-09-29 03:12" --view jobs
```

Four views of one moment: the units, the processes, the jobs that ran in
the quarter of an hour before, and the processes that ended in it. Under
the units and the processes is the selected row's last ten minutes.

| key | |
|---|---|
| `←` `→` | ten seconds back, forward |
| `,` `.` | a minute |
| `<` `>` | ten minutes |
| `[` `]` | an hour |
| `{` `}` | a day |
| `home` | the first moment in the store |
| `l`, `end` | now |
| `t` | go to a moment, typed: `-15m`, `14:30`, `2026-09-29 14:30` |
| `m` | go to the moment of the selected exit or job |
| `-` `+` | a longer stretch of the timeline, a shorter: ten minutes to a week |
| `tab`, `1` to `4` | the view |
| `↑` `↓`, `j` `k` | the row |
| `enter` | open the row: a unit into its processes, a process or an exit into what is known of it |
| `esc` | back out of a unit |
| `s` | sort by cpu, memory, i/o, name |
| `a` | show slices too |
| `/` | show only what matches |
| `q` | quit |

Across the top is the timeline: how busy the host was over the last hour,
with `▲` under the moment looked at, `!` where a process ended by a fault,
and `·` where one was killed. Looking for what went wrong is looking along
it; or finding it among the exits, with `/` and `SIGSEGV`, and pressing
`m` to see the host as it was then.

`/` looks for what matches, in what is on the screen as it is typed, and
with `enter` in the store as far back as the timeline shows: a busy host
ends hundreds of processes a minute, and the one looked for is seldom
among the last few.

The jobs that are running are first among the jobs, while now is what is
looked at, with how long each has taken so far.

**Now is read from the kernel**, by the collectors the store is filled by,
every two seconds. It is on the screen as it happens, and needs no
collector to be running. **Every other moment is read from the store.**

Two things follow. Going back from now lands on the last moment the store
holds, which is up to a minute ago: the collector flushes once a minute.
And now is read as whoever is watching, so without the collector's
privileges the I/O of other users' processes is not shown; the moments in
the store have it.

The jobs and the exits are the store's in every view, now included.

Opening a process shows what it ran, from where, in which unit, and as
whom; and, if it has ended since the moment looked at, how. Going into a
unit and then back through time stays in the unit.

Slices are left out of the units unless asked for with `a`. A slice is the
sum of the units in it, and at the top of a list by size it says what the
rest of the list says again.

`watch --print 120x40` draws the screen once, as text, for a script or for
where there is no terminal.

## Reading a local store

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

## Cost

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

### What the store costs in memory

With a store of its own, the collector's memory is mostly the engine's
index of that store, and grows with it.

| | |
|---|---|
| a series in the store | about 1.4 KB, for as long as the store keeps it |
| a chunk | about 225 bytes; a flush writes one for each series with samples, which was 1.2 MB a minute, until compaction merges them |
| a series that has ended, with the two or three chunks left of it | about 1.9 KB |
| a maintenance pass | up to 130 MB while it runs, given back when it ends |

A process that lives for thirty seconds is fifteen series, so the number
of series is the number of processes there have been, and not the number
there are. The workstation above made between 1,000 and 4,000 series an
hour. At 1.9 KB each that is 50 to 180 MB a day, for the thirty days raw
samples are kept. That figure is worked out from an hour and a half, not
measured over a month. It is the engine's to change:
[timeless-libsql#82](https://github.com/awksedgreep/timeless-libsql/issues/82).

Half the series of that store never held a value but zero: a process that
never swapped, never faulted a page in from disk, never read or wrote.
They are stored all the same, so that a reader is told nothing happened
and does not have to infer it.

Pushing to the planes moves all of this to the planes.

### What a sample costs to store

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
last row. [DESIGN.md](DESIGN.md#why-every-hour) has the measurements and
the reasoning.

Each series also costs about 250 bytes once, for its name and labels, and
each compaction writes one chunk per series per rollup tier.

What moves the cost, in order: `--maintain-interval` for a local store,
`--process-interval` (the process and unit tiers are nine tenths of the
samples), `--min-age` (fewer processes with series of their own), and
`--retention`.

Through the planes, chunking is the planes' own: they compact every five
minutes.

## Options

`timeless-acct run --help` lists them all. The ones that matter:

| option | default | |
|---|---|---|
| `--sink` | `embedded` | `embedded`, `http`, or `stdout` |
| `--host` | the hostname | the name this host is recorded under |
| `--interval` | 10 | seconds between readings of the system |
| `--process-interval` | 10 | seconds between sweeps of the processes |
| `--min-age` | 30 | seconds a process must have lived to get series of its own |
| `--kernel-threads` | off | give kernel threads series of their own |
| `--no-units` | | do not report units |
| `--no-exec-events` | | do not ask the kernel for word of each exec |
| `--no-traces` | | do not keep a span for each process that ends |
| `--trace-max-age` | `1h` | how long after a job starts a process may start and be part of its trace |
| `--retention` | `30d` | local store: how long samples are kept |
| `--rollups` | `5m@180d,1h@0` | local store: coarser copies kept after that |
| `--log-retention` | `90d` | local store: how long accounting records are kept |
| `--trace-retention` | `30d` | local store: how long spans are kept |
| `--token` | | planes: a bearer token, if they require one |

Retention and rollups apply when a store is created. For the planes, they
are the planes' own settings.

If a plane is unreachable, the collector keeps up to an hour of ticks and
sends them, in order and at the times they were taken, when it answers.

## What is not here yet

- **Units where systemd does not name them.** A unit is recognized by its
  name: `.service`, `.scope`, `.slice`. Containers run by Docker with its
  own control group driver, or by Kubernetes, are in groups named otherwise
  and are not reported.
- **Containers that no unit runs.** One started by hand, with `podman run`
  or `docker run`, is in a scope named for its id and nothing else, and
  all of them are reported together as `libpod.scope` or `docker.scope`.
  Naming them takes asking the runtime.
- **Secrets that are not named for what they are.** See
  [Secrets](#secrets).
- **What a forked child turned itself into.** A postgres worker renames
  itself after it starts. Its record has what its parent was running.
- **Jobs longer than an hour.** What a job starts after its first hour is
  a trace of its own, and a job that has been running for longer is not
  among the jobs that are running.
- **A bound on memory**, with a store of its own. See
  [what the store costs in memory](#what-the-store-costs-in-memory).
- **The end of a series.** Nothing marks a process's series as over, so a
  reader has to be told how far back to look: see
  [Reading with PromQL](#reading-with-promql). It takes the plane as well:
  [timeless-libsql#83](https://github.com/awksedgreep/timeless-libsql/issues/83).
- **Fans and batteries** are collected where the kernel has them, and were
  tested against a host that has neither.
- **Proportional memory** (PSS). `proc_rss_bytes` counts shared pages once
  per process that maps them.

[DESIGN.md](DESIGN.md#what-is-not-here-yet) has the reasoning and the order.

## Testing

```sh
cargo test
```

Parsers are tested against captured kernel text, collectors against a
fixture `/proc`, the taskstats decoder against records built to the
kernel's layout, and the local store against the real engine.

## License

[MIT](LICENSE)
