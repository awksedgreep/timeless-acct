# What is recorded

Every metric is a gauge, with the rate already taken: the canvas draws the
last value in a bucket, and sar has always recorded rates. Every series
carries `host`.

## The system: `sys_*`

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

## Processes: `proc_*`

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

## Units: `unit_*`

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

## Totals: `procgroup_*` and `procuser_*`

Over **every** process, including those too young or too brief for series
of their own, by command name (`comm`) and by user (`user`).

| metric | is |
|---|---|
| `procgroup_processes`, `procgroup_threads`, `procgroup_rss_bytes` | now |
| `procgroup_cpu_pct` | share of one CPU over the interval, including processes that ended in it |
| `procgroup_io_{read,write}_bytes_per_sec` | |
| `procuser_processes`, `procuser_rss_bytes`, `procuser_cpu_pct` | the same, by user |

## The collector: `acct_*`

`acct_processes`, `acct_processes_reported`, `acct_sweep_seconds`,
`acct_exits`, `acct_exits_lost`, `acct_execs`, `acct_execs_missed`,
`acct_execs_lost`, `acct_interval_seconds`,
`acct_process_interval_seconds`,
`acct_written_bytes_total{plane="metrics"|"logs"|"traces"}`; and with a
store of its own, `acct_store_bytes` and `acct_store_limit_bytes`.

`rate(acct_written_bytes_total[1h])` against `rate(acct_store_bytes[1h])`
is what the same data costs on the wire against what it costs kept.

The two intervals are what the collector was started with, so that a
reader elsewhere can take its lookback from the store rather than be
told: three times `acct_process_interval_seconds`, by host.

A rising `acct_exits_lost` means processes are ending faster than their
records can be read. Up to 262,144 wait between two sweeps, which at the
default interval is 26,000 tasks ending a second; a browser being compiled
on 22 CPUs ended 470 processes a second at its busiest.
`acct_execs_missed` counts processes that were gone
before they could be described: their records have a name and no
arguments.

## Accounting records

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

## Secrets

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

## Traces

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
