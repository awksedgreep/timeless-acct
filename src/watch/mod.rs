//! Watching a host in a terminal: now, and at any moment the store holds.
//!
//! This is what the canvas is for, in the place a collector already is.
//! Now is read from the kernel by the collectors themselves, so it is on
//! the screen as it happens. Every other moment is read from the store.

mod data;
mod running;
mod state;
mod store;
mod view;

use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::Terminal;

use crate::accounting::{human_bytes, human_duration};
use crate::cli::WatchArgs;
use crate::clock;
use crate::collect::process::{ProcessCollector, ProcessOptions, Units};
use crate::collect::system::{SystemCollector, SystemOptions};
use crate::collect::units::UnitCollector;
use crate::model::MetricBatch;
use crate::procfs::process::parse_pid_stat;
use crate::procfs::system::parse_stat;
use crate::procfs::ProcRoot;
use crate::taskstats::epoch_now;

use data::{Sampled, Snapshot};
use state::{Changed, State, Tab};
use store::{Reach, Store};
use view::{draw, history_of, Detail};

/// How far back a series' last sample may be for the series to be there.
/// Three of the store's samples: a process that has ended has none.
const WITHIN: f64 = 30.0;
/// How far back a row's history goes.
const HISTORY: f64 = 600.0;
/// How far back jobs and exits are looked for.
const RECENT: f64 = 900.0;
const ROWS: usize = 200;
/// How long a job may have been running and still be one: the store's
/// rule for what is one trace.
const JOB: f64 = 3600.0;
/// How long the timeline is kept before it is read again, while now is
/// what is looked at.
const TIMELINE: Duration = Duration::from_secs(10);

/// Now, read from the kernel.
struct Live {
    system: SystemCollector,
    processes: ProcessCollector,
    units: Option<UnitCollector>,
}

impl Live {
    fn new() -> Result<Self> {
        let root = ProcRoot::default();
        let stat = root.read("stat").context("read /proc/stat")?;
        let boot = parse_stat(&stat).boot_time as f64;
        let options = ProcessOptions {
            // Everything is on the screen that is there to be seen; what is
            // worth a series of its own is the store's concern.
            min_age: 0.0,
            ..ProcessOptions::default()
        };
        Ok(Self {
            system: SystemCollector::new(root.clone(), SystemOptions::default()),
            processes: ProcessCollector::new(root.clone(), options, Units::from_system(), boot),
            units: UnitCollector::new(root),
        })
    }

    /// The jobs that are running, as of the last reading.
    fn jobs(&self, width: usize) -> Vec<store::Job> {
        running::running(self.processes.all(), epoch_now(), JOB, width)
    }

    /// A reading. The first has nothing to take a difference against, and
    /// has levels and no rates.
    fn read(&mut self) -> Snapshot {
        let now = Instant::now();
        let wall = epoch_now();
        let mut batch = MetricBatch::new(wall as i64);
        self.system.collect(now, &mut batch);
        let sweep = self.processes.sweep(now, wall, &[], &mut batch);
        if let Some(units) = &mut self.units {
            self.processes.set_containers(units.containers());
            units.collect(now, &sweep.cgroups, &mut batch);
        }
        Snapshot::read(wall, &mut Sampled::new(&batch))
    }
}

/// Everything the screen shows, and where it is read from.
struct Watch {
    store: Store,
    live: Live,
    state: State,
    snapshot: Snapshot,
    detail: Detail,
    width: usize,
    /// When the timeline was read, and of which stretch.
    timeline_read: Option<(Instant, (f64, f64))>,
}

impl Watch {
    fn new(args: &WatchArgs) -> Result<Self> {
        let store = Store::open(&args.data_dir, args.host.as_deref())?;
        let now = epoch_now();
        let at = match args.at.as_str() {
            "now" => None,
            text => Some(clock::parse(text, now)?),
        };
        let mut state = State::new(at, 10.0);
        state.tab = match args.view {
            crate::cli::WatchView::Units => Tab::Units,
            crate::cli::WatchView::Processes => Tab::Processes,
            crate::cli::WatchView::Jobs => Tab::Jobs,
            crate::cli::WatchView::Exits => Tab::Exits,
        };
        let mut live = Live::new()?;
        // The reading the first one on the screen is a difference from.
        live.read();
        Ok(Self {
            store,
            live,
            state,
            snapshot: Snapshot::default(),
            detail: Detail::default(),
            width: 100,
            timeline_read: None,
        })
    }

    /// Read the moment looked at.
    fn read_moment(&mut self) {
        self.detail.now = epoch_now();
        self.detail.range = self.store.range();
        self.snapshot = match self.state.at {
            None => self.live.read(),
            Some(at) => Snapshot::read(at, &mut self.store.at(at, WITHIN)),
        };
        self.read_detail();
    }

    /// Read the timeline: how busy the host was, and what went wrong,
    /// over a stretch that has the moment looked at in it.
    fn read_timeline(&mut self) {
        let span = self.state.window();
        let last = self.detail.range.map_or(self.detail.now, |(_, last)| last);
        // Up to now, if the moment is within the stretch before now; and
        // around the moment, if it is further back than that.
        let to = match self.state.at {
            Some(at) if at < last - span => (at + span / 2.0).min(last),
            Some(_) => last,
            None => self.detail.now,
        };
        // A stretch is drawn a column to so many seconds; a stretch that
        // has moved by less than a column is the same stretch.
        let column = span / 100.0;
        let to = (to / column).ceil() * column;
        let window = (to - span, to);
        let fresh = self.timeline_read.is_some_and(|(at, read)| {
            read == window && (!self.state.is_live() || at.elapsed() < TIMELINE)
        });
        if fresh {
            return;
        }
        let (timeline, step) = self.store.timeline(window.0, window.1);
        self.detail.timeline = timeline;
        self.detail.timeline_step = step;
        self.detail.incidents = self.store.incidents(window.0, window.1);
        self.detail.window = window;
        self.timeline_read = Some((Instant::now(), window));
    }

    /// Read what goes with the moment: the selected row's history, and
    /// what ran and what ended in the minutes before.
    fn read_detail(&mut self) {
        self.detail.now = epoch_now();
        self.detail.error = None;
        self.read_timeline();
        // Up to the moment looked at; and now, as far as the store goes.
        let until = self
            .state
            .at
            .or(self.detail.range.map(|(_, last)| last))
            .unwrap_or(self.detail.now);

        self.detail.history.clear();
        self.detail.history_of.clear();
        self.detail.history_span = HISTORY;
        self.detail.history_until = until;
        self.detail.step = self.state.step;
        // The quarter of an hour before; and, when something is looked
        // for, as far back as the timeline shows.
        let reach = Reach {
            until,
            span: if self.state.is_looking() {
                self.state.window().max(RECENT)
            } else {
                RECENT
            },
            limit: ROWS,
        };
        let state = &self.state;
        match self.state.tab {
            Tab::Units | Tab::Processes => {
                let rows = match self.state.tab {
                    Tab::Units => self.state.units(&self.snapshot).len(),
                    _ => self.state.processes(&self.snapshot).len(),
                };
                self.state.clamp(rows);
                if let Some((metric, key, want, title)) = history_of(&self.state, &self.snapshot) {
                    self.detail.history =
                        self.store
                            .history(metric, key, &want, until - HISTORY, until);
                    self.detail.history_of = title;
                }
            }
            Tab::Jobs => match self.store.jobs(reach, self.width, &|job| {
                state.wants(&[&job.command, &job.unit])
            }) {
                Ok(ended) => {
                    // What is running is first, while now is what is
                    // looked at. At any other moment, what was running
                    // then has ended since, or is among these still.
                    let mut jobs = if self.state.is_live() {
                        self.live.jobs(self.width)
                    } else {
                        Vec::new()
                    };
                    jobs.extend(ended);
                    self.detail.jobs = jobs;
                }
                Err(error) => self.detail.error = Some(format!("{error:#}")),
            },
            Tab::Exits => match self.store.exits(reach, &|exit| {
                state.wants(&[&exit.command, &exit.unit, &exit.status, &exit.user])
            }) {
                Ok(exits) => self.detail.exits = exits,
                Err(error) => self.detail.error = Some(format!("{error:#}")),
            },
        }
    }
}

/// What there is to say of a process that has ended, from its record.
fn ended(at: f64, record: &serde_json::Value) -> Vec<(&'static str, String)> {
    let text = |key: &str| record[key].as_str().unwrap_or("").to_string();
    let number = |key: &str| record[key].as_f64();
    let mut lines = Vec::new();
    let mut say = |label: &'static str, value: String| {
        if !value.is_empty() {
            lines.push((label, value));
        }
    };
    say("ran", text("cmdline"));
    say("started as", text("started_as"));
    say("from", text("path"));
    say("in", text("unit"));
    say("as", text("user"));
    if let Some(started) = number("started") {
        say("started", clock::format(started));
    }
    let how = match text("status").as_str() {
        "unknown" => "was gone".to_string(),
        "0" => "exited 0".to_string(),
        code if code.bytes().all(|b| b.is_ascii_digit()) => format!("exited {code}"),
        signal if record["core_dumped"] == true => format!("killed by {signal}, core dumped"),
        signal => format!("killed by {signal}"),
    };
    say(
        "ended",
        format!(
            "{}: {how}, after {}",
            clock::format(at),
            human_duration(number("elapsed_seconds").unwrap_or(0.0))
        ),
    );
    if let Some(cpu) = number("cpu_seconds") {
        let share = number("cpu_pct").map_or(String::new(), |pct| format!(", {pct:.0}% of a CPU"));
        say("cpu", format!("{}{share}", human_duration(cpu)));
    }
    if let Some(peak) = record["peak_rss_bytes"].as_u64() {
        say("peak memory", human_bytes(peak));
    }
    if let (Some(read), Some(written)) = (
        record["io_read_bytes"].as_u64(),
        record["io_write_bytes"].as_u64(),
    ) {
        say(
            "storage",
            format!("read {}, wrote {}", human_bytes(read), human_bytes(written)),
        );
    }
    if let Some(threads) = record["threads"].as_u64().filter(|n| *n > 1) {
        say("threads", threads.to_string());
    }
    if record["forked"] == true {
        say(
            "note",
            "It never called exec: what it ran is what its parent was running.".into(),
        );
    }
    if record["source"] == "sampled" {
        say(
            "note",
            "The kernel's record of its end was not received: these are the figures of the \
             last sweep that saw it."
                .into(),
        );
    }
    lines
}

/// What there is to say of a process that is running, from the kernel.
fn running(root: &ProcRoot, pid: u32, name: &str) -> Option<Vec<(&'static str, String)>> {
    let mut buf = String::new();
    root.read_pid(pid, "stat", &mut buf).ok()?;
    // The same pid, and another process: the one looked for has ended.
    if parse_pid_stat(&buf)?.comm != name {
        return None;
    }
    let described = root.describe(pid, 4096, &mut buf);
    let mut lines = Vec::new();
    for (label, value) in [("ran", described.cmdline), ("from", described.exe)] {
        if !value.is_empty() {
            lines.push((label, value));
        }
    }
    lines.push(("ended", "It is still running.".into()));
    Some(lines)
}

impl Watch {
    /// Go to the moment of the selected row: when the process ended, or
    /// when the job began.
    fn go(&mut self) {
        let selected = self.state.selected();
        let at = match self.state.tab {
            Tab::Exits => self
                .detail
                .exits
                .iter()
                .filter(|exit| {
                    self.state
                        .wants(&[&exit.command, &exit.unit, &exit.status, &exit.user])
                })
                .nth(selected)
                .map(|exit| exit.at),
            Tab::Jobs => self
                .detail
                .jobs
                .iter()
                .filter(|job| self.state.wants(&[&job.command, &job.unit]))
                .nth(selected)
                .map(|job| job.started),
            Tab::Units | Tab::Processes => None,
        };
        match at {
            Some(at) => self.state.go_to(at, self.detail.range),
            None => {
                self.state.message =
                    Some("An exit or a job has a moment to go to: views 3 and 4.".into())
            }
        }
    }

    /// Open the selected row.
    fn open(&mut self) {
        match self.state.tab {
            Tab::Units => {
                let rows = self.state.units(&self.snapshot);
                if let Some(unit) = rows.get(self.state.selected()) {
                    let unit = unit.name.clone();
                    self.state.enter(unit);
                }
            }
            Tab::Processes => {
                let rows = self.state.processes(&self.snapshot);
                let Some(process) = rows.get(self.state.selected()) else {
                    return;
                };
                let pid: u32 = process.pid.parse().unwrap_or(0);
                let mut lines = vec![];
                if !process.unit.is_empty() {
                    lines.push(("in", process.unit.clone()));
                }
                lines.push(("as", process.user.clone()));
                // How it ended, if it has since the moment looked at; and
                // what it is running, if it has not.
                let from = self.state.at.unwrap_or(self.detail.now);
                let known = match self.store.record(&process.comm, u64::from(pid), from) {
                    Some((at, record)) => ended(at, &record),
                    None => {
                        running(&ProcRoot::default(), pid, &process.comm).unwrap_or_else(|| {
                            vec![(
                                "ended",
                                "It has ended, and its record is not in the store yet.".into(),
                            )]
                        })
                    }
                };
                // What its record says of where and as whom is later, and
                // is what is kept.
                lines.retain(|(label, _)| !known.iter().any(|(other, _)| other == label));
                let ran: Vec<_> = known.iter().filter(|(l, _)| *l == "ran").cloned().collect();
                let rest = known.into_iter().filter(|(l, _)| *l != "ran");
                let lines = ran.into_iter().chain(lines).chain(rest).collect();
                self.detail.inspected = Some((process.name.clone(), lines));
                self.state.inspecting = true;
            }
            Tab::Exits => {
                let exit = self
                    .detail
                    .exits
                    .iter()
                    .filter(|exit| {
                        self.state
                            .wants(&[&exit.command, &exit.unit, &exit.status, &exit.user])
                    })
                    .nth(self.state.selected());
                let Some(exit) = exit else {
                    return;
                };
                if let Some((at, record)) = self.store.record(&exit.name, exit.pid, exit.at - 1.0) {
                    self.detail.inspected =
                        Some((format!("{}[{}]", exit.name, exit.pid), ended(at, &record)));
                    self.state.inspecting = true;
                }
            }
            Tab::Jobs => {}
        }
    }
}

/// Put the terminal back as it was, whatever happens.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn watch(args: &WatchArgs) -> Result<()> {
    if args.refresh < 1 {
        bail!("--refresh is a number of seconds, and at least one");
    }
    let mut watch = Watch::new(args)?;
    if let Some(size) = &args.print {
        return print(&mut watch, size);
    }
    if !io::stdout().is_terminal() {
        bail!("not a terminal: `watch --print` draws the screen once, as text");
    }

    enable_raw_mode().context("take the terminal")?;
    let _restore = Restore;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        drop(Restore);
        hook(panic);
    }));
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let refresh = Duration::from_secs(args.refresh);
    // The first reading on the screen is the second taken: a moment's
    // wait gives it an interval to have rates over.
    std::thread::sleep(Duration::from_millis(500));
    watch.read_moment();
    let mut read_at = Instant::now();

    while !watch.state.quit {
        watch.width = usize::from(terminal.size()?.width)
            .saturating_sub(40)
            .max(40);
        terminal.draw(|frame| {
            draw(frame, &mut watch.state, &watch.snapshot, &watch.detail);
        })?;

        let wait = if watch.state.is_live() {
            refresh.saturating_sub(read_at.elapsed())
        } else {
            Duration::from_secs(1)
        };
        if event::poll(wait.max(Duration::from_millis(20)))? {
            // Every key that is waiting, before anything is read: holding
            // an arrow down goes through time without reading each moment
            // passed on the way.
            let mut changed = Changed::Nothing;
            while event::poll(Duration::ZERO)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    match watch.state.key(key, epoch_now(), watch.detail.range) {
                        Changed::Moment => changed = Changed::Moment,
                        Changed::View if changed == Changed::Nothing => changed = Changed::View,
                        Changed::Go => {
                            watch.go();
                            changed = Changed::Moment;
                        }
                        Changed::Open => {
                            watch.open();
                            // Into a unit is into another view, with
                            // another row's history to read.
                            changed = Changed::Moment;
                        }
                        _ => {}
                    }
                }
            }
            if changed == Changed::Moment {
                if watch.state.is_live() && read_at.elapsed() < refresh {
                    // Now has been read already; only what goes with it
                    // has changed.
                    watch.read_detail();
                } else {
                    watch.read_moment();
                    read_at = Instant::now();
                }
            }
        } else if watch.state.is_live() {
            watch.read_moment();
            read_at = Instant::now();
        } else {
            // A moment in the past does not change. How long ago it was
            // does, and how far the store goes.
            watch.detail.now = epoch_now();
            watch.detail.range = watch.store.range();
        }
    }
    Ok(())
}

/// Draw the screen once, as text: for a script, or for where there is no
/// terminal to draw on.
fn print(watch: &mut Watch, size: &str) -> Result<()> {
    let (width, height) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?)))
        .filter(|(w, h)| *w >= 40 && *h >= 12)
        .with_context(|| {
            format!("{size:?} is not a size: expected WIDTHxHEIGHT, at least 40x12")
        })?;
    watch.width = usize::from(width).saturating_sub(40).max(40);
    if watch.state.is_live() {
        std::thread::sleep(Duration::from_secs(1));
    }
    watch.read_moment();

    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| {
        draw(frame, &mut watch.state, &watch.snapshot, &watch.detail);
    })?;
    let buffer = terminal.backend().buffer();
    for row in buffer.content.chunks(usize::from(width)) {
        let line: String = row.iter().map(|cell| cell.symbol()).collect();
        println!("{}", line.trim_end());
    }
    Ok(())
}
