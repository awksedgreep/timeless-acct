# Watching a store

```sh
timeless-acct watch                                       # now
timeless-acct watch --at "2026-09-29 03:12" --view jobs   # a moment, and a view
timeless-acct watch --data-dir /srv/copies/web-1          # another store
```

It reads your own store if you have one, and the host's otherwise: see
[where the store is](running.md#where-the-store-is).

Four views of one moment: the units, the processes, the jobs that ran in
the quarter of an hour before, and the processes that ended in it. Under
the units and the processes is the selected row's last ten minutes.

| key | |
|---|---|
| `←` `→` | one sample back, forward: ten seconds, unless the collector was told otherwise |
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
| `esc` | back: from what is looked for, then out of a unit, then out of the viewer |
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
among the last few. An exit matches by what it ran, how it ended, its
unit, or its user; a job, by any of those of any process in it, so a
build is found by its compiler.

What matches by the command's name or by how it ended is on the screen at
once. The rest is read for, a page of records at a time, and the screen
says how far back it has read and answers keys while it does: over an
hour in which a quarter of a million processes ended, seven seconds if
there is nothing to find, and less than one if there is a screen's worth.
`esc` puts all of them back.

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
