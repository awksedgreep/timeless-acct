//! Drawing the screen.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Sparkline, Table, TableState};
use ratatui::Frame;

use crate::accounting::{human_bytes, human_duration};
use crate::clock;

use super::data::Snapshot;
use super::state::{Sort, State, Tab};
use super::store::{ago, Exit, Incident, Job};

/// What is read for the screen besides the moment itself.
#[derive(Debug, Default)]
pub struct Detail {
    /// Epoch seconds, as of the last look at the clock.
    pub now: f64,
    /// The first and last moments the store holds.
    pub range: Option<(f64, f64)>,
    /// The selected row's figure over the minutes up to the moment, and
    /// what it is a figure of.
    pub history: Vec<(f64, f64)>,
    pub history_of: String,
    /// The stretch the history is of: its length in seconds, and the
    /// moment it runs up to.
    pub history_span: f64,
    pub history_until: f64,
    pub jobs: Vec<Job>,
    pub exits: Vec<Exit>,
    /// How busy the host was over the stretch the timeline shows, how far
    /// apart the figures are, and the stretch: from, to.
    pub timeline: Vec<(f64, f64)>,
    pub timeline_step: f64,
    pub window: (f64, f64),
    /// What went wrong in it.
    pub incidents: Vec<Incident>,
    /// What is known of the row that was opened: a title, and what there
    /// is to say under it.
    pub inspected: Option<(String, Vec<(&'static str, String)>)>,
    /// Seconds between the store's samples.
    pub step: f64,
    /// How far back the records have been read so far, in epoch seconds,
    /// while there is further to read: what is on the screen is what has
    /// been found by then.
    pub looking: Option<f64>,
    /// How far back jobs and exits are looked for, in seconds.
    pub reach: f64,
    /// What went wrong reading the store, if something did.
    pub error: Option<String>,
}

const DIM: Style = Style::new().fg(Color::DarkGray);
const HEAD: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const BAD: Style = Style::new().fg(Color::Red);
const WARN: Style = Style::new().fg(Color::Yellow);
const GOOD: Style = Style::new().fg(Color::Green);
const PICKED: Style = Style::new().add_modifier(Modifier::REVERSED);

fn figure(value: Option<f64>, show: impl Fn(f64) -> String) -> String {
    value.map_or_else(|| "-".into(), show)
}

fn percent(value: Option<f64>) -> String {
    figure(value, |v| format!("{v:.1}"))
}

fn bytes(value: Option<f64>) -> String {
    figure(value, |v| human_bytes(v.max(0.0) as u64))
}

fn count(value: Option<f64>) -> String {
    figure(value, |v| format!("{v:.0}"))
}

/// A figure that is worth a colour when it is high.
fn heat(value: Option<f64>, warm: f64, hot: f64) -> Style {
    match value {
        Some(v) if v >= hot => BAD,
        Some(v) if v >= warm => WARN,
        _ => Style::new(),
    }
}

pub fn draw(frame: &mut Frame, state: &mut State, snapshot: &Snapshot, detail: &Detail) {
    let [header, tabs, body, keys] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, state, snapshot, detail);
    draw_tabs(frame, tabs, state, detail);
    match state.tab {
        Tab::Units | Tab::Processes => {
            let [table, history] =
                Layout::vertical([Constraint::Min(3), Constraint::Length(6)]).areas(body);
            if state.tab == Tab::Units {
                draw_units(frame, table, state, snapshot);
            } else {
                draw_processes(frame, table, state, snapshot);
            }
            draw_history(frame, history, detail);
        }
        Tab::Jobs => draw_jobs(frame, body, state, detail),
        Tab::Exits => draw_exits(frame, body, state, detail),
    }
    draw_keys(frame, keys, state, detail);
    if state.help {
        draw_help(frame);
    }
    if state.inspecting {
        if let Some((title, lines)) = &detail.inspected {
            draw_inspected(frame, title, lines);
        }
    }
}

fn draw_header(frame: &mut Frame, area: Rect, state: &State, snapshot: &Snapshot, detail: &Detail) {
    let when = match state.at {
        None => Line::from(vec![
            Span::styled(" ● LIVE ", GOOD.add_modifier(Modifier::BOLD)),
            Span::styled(clock::format(detail.now), DIM),
            Span::raw(" "),
        ]),
        Some(at) => Line::from(vec![
            Span::styled(" ◀ ", WARN),
            Span::styled(clock::format(at), WARN.add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {} ", ago(detail.now, at)), WARN),
        ]),
    };
    let (from, to) = detail.window;
    // A stretch within a day is told by the time; a longer one needs the
    // day as well.
    let tell = |at: f64| -> String {
        let text = clock::format(at);
        if to - from > 86_400.0 {
            text[5..16].to_string()
        } else {
            text[11..16].to_string()
        }
    };
    let mut block = Block::bordered()
        .title(Span::styled(" timeless-acct ", HEAD))
        .title(when.right_aligned());
    if to > from {
        block = block
            .title_bottom(Span::styled(format!(" {} ", tell(from)), DIM))
            .title_bottom(
                Line::from(Span::styled(
                    format!(
                        " cpu over {}, up to {:.1}% ",
                        human_duration(to - from),
                        detail
                            .timeline
                            .iter()
                            .map(|(_, busy)| *busy)
                            .fold(0.0, f64::max)
                    ),
                    DIM,
                ))
                .centered(),
            )
            .title_bottom(Line::from(Span::styled(format!(" {} ", tell(to)), DIM)).right_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [inner, busy, marked] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    if to > from {
        let width = usize::from(busy.width);
        let data = columns(&detail.timeline, to, to - from, width, detail.timeline_step);
        frame.render_widget(
            // Drawn to the highest in the stretch, which the frame says:
            // a host is seldom near all of its CPUs, and drawn to a
            // hundred its busiest hour would be a flat line.
            Sparkline::default()
                .data(&data)
                .style(Style::new().fg(Color::Blue)),
            busy,
        );
        let cursor = state.at.unwrap_or(to);
        let line: Vec<Span> = marks(width, from, to, cursor, &detail.incidents)
            .into_iter()
            .map(|mark| match mark {
                Mark::Nothing => Span::raw(" "),
                Mark::Killed => Span::styled("·", WARN),
                Mark::Fault => Span::styled("!", BAD.add_modifier(Modifier::BOLD)),
                Mark::Here => Span::styled("▲", HEAD),
                Mark::HereKilled => Span::styled("▲", WARN),
                Mark::HereFault => Span::styled("▲", BAD),
            })
            .collect();
        frame.render_widget(Paragraph::new(Line::from(line)), marked);
    }

    if snapshot.is_empty() {
        let why = match (state.at, detail.range) {
            (Some(_), None) | (None, None) => "The store holds nothing yet.".to_string(),
            (Some(at), Some((first, _))) if at < first => "Before the store began.".to_string(),
            _ => "Nothing was recorded at this moment: the collector was not running.".to_string(),
        };
        frame.render_widget(Paragraph::new(Span::styled(why, WARN)), inner);
        return;
    }

    let s = &snapshot.system;
    let label = |text: &'static str| Span::styled(text, DIM);
    let mut first = vec![label("load ")];
    first.push(Span::raw(match s.load {
        Some([a, b, c]) => format!("{a:.2} {b:.2} {c:.2}"),
        None => "-".into(),
    }));
    first.push(label("   cpu "));
    first.push(Span::styled(
        format!("{}%", percent(s.cpu_busy)),
        heat(s.cpu_busy, 60.0, 85.0),
    ));
    first.push(label(" (user "));
    first.push(Span::raw(percent(s.cpu_user)));
    first.push(label(" sys "));
    first.push(Span::raw(percent(s.cpu_system)));
    first.push(label(" wait "));
    first.push(Span::raw(percent(s.cpu_iowait)));
    first.push(label(")   mem "));
    let used = match (s.mem_used, s.mem_total) {
        (Some(used), Some(total)) if total > 0.0 => Some(100.0 * used / total),
        _ => None,
    };
    first.push(Span::styled(
        format!("{} of {}", bytes(s.mem_used), bytes(s.mem_total)),
        heat(used, 80.0, 92.0),
    ));
    if s.swap_used.is_some_and(|used| used > 0.0) {
        first.push(label("   swap "));
        first.push(Span::raw(bytes(s.swap_used)));
    }

    let mut second = vec![label("disk ")];
    second.push(Span::raw(format!(
        "↓{}/s ↑{}/s",
        bytes(s.io_read),
        bytes(s.io_write)
    )));
    second.push(label("   net "));
    second.push(Span::raw(format!(
        "↓{}/s ↑{}/s",
        bytes(s.net_rx),
        bytes(s.net_tx)
    )));
    second.push(label("   tasks "));
    second.push(Span::raw(count(s.tasks)));
    if s.temp.is_some() {
        second.push(label("   "));
        second.push(Span::styled(
            format!("{}°C", figure(s.temp, |v| format!("{v:.0}"))),
            heat(s.temp, 80.0, 92.0),
        ));
    }
    if s.power.is_some() {
        second.push(label(" "));
        second.push(Span::raw(format!(
            "{}W",
            figure(s.power, |v| format!("{v:.0}"))
        )));
    }
    second.push(label("   stalled: cpu "));
    for (index, name) in ["", " mem ", " i/o "].into_iter().enumerate() {
        if !name.is_empty() {
            second.push(label(name));
        }
        second.push(Span::styled(
            format!("{}%", percent(s.pressure[index])),
            heat(s.pressure[index], 10.0, 40.0),
        ));
    }
    frame.render_widget(
        Paragraph::new(vec![Line::from(first), Line::from(second)]),
        inner,
    );
}

/// What is under one column of the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Nothing,
    /// A process was killed.
    Killed,
    /// A process ended by a fault.
    Fault,
    /// The moment looked at.
    Here,
    HereKilled,
    HereFault,
}

/// What to put under each column of the timeline: what went wrong in the
/// time it stands for, and where the moment looked at is.
pub fn marks(width: usize, from: f64, to: f64, cursor: f64, incidents: &[Incident]) -> Vec<Mark> {
    let mut marks = vec![Mark::Nothing; width];
    if width == 0 || to <= from {
        return marks;
    }
    let column = |at: f64| -> Option<usize> {
        let position = (at - from) / (to - from) * width as f64;
        (position >= 0.0 && at <= to).then(|| (position as usize).min(width - 1))
    };
    for incident in incidents {
        if let Some(column) = column(incident.at) {
            // A fault is not hidden by a kill in the same column.
            if incident.error {
                marks[column] = Mark::Fault;
            } else if marks[column] == Mark::Nothing {
                marks[column] = Mark::Killed;
            }
        }
    }
    if let Some(column) = column(cursor) {
        marks[column] = match marks[column] {
            Mark::Fault => Mark::HereFault,
            Mark::Killed => Mark::HereKilled,
            _ => Mark::Here,
        };
    }
    marks
}

fn draw_tabs(frame: &mut Frame, area: Rect, state: &State, detail: &Detail) {
    let mut spans = Vec::new();
    for (index, tab) in Tab::ALL.into_iter().enumerate() {
        let text = format!(" {} {} ", index + 1, tab.title());
        spans.push(if tab == state.tab {
            Span::styled(text, HEAD.add_modifier(Modifier::REVERSED))
        } else {
            Span::styled(text, DIM)
        });
    }
    if matches!(state.tab, Tab::Units | Tab::Processes) {
        spans.push(Span::styled("   by ", DIM));
        spans.push(Span::raw(state.sort.title()));
    }
    if state.tab == Tab::Units && state.sums {
        spans.push(Span::styled("   with slices", DIM));
    }
    if let (Tab::Processes, Some(unit)) = (state.tab, &state.within) {
        spans.push(Span::styled("   in ", DIM));
        spans.push(Span::styled(unit.clone(), HEAD));
    }
    if state.typing || !state.filter.is_empty() {
        spans.push(Span::styled("   only ", DIM));
        spans.push(Span::styled(
            format!("{}{}", state.filter, if state.typing { "▏" } else { "" }),
            WARN,
        ));
    }
    if let (Tab::Exits, Some(reached)) = (state.tab, detail.looking) {
        spans.push(Span::styled(
            format!("   read back to {} …", &clock::format(reached)[11..]),
            DIM,
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A table with its header, and the selected row kept in sight.
fn table<'a>(
    frame: &mut Frame,
    area: Rect,
    state: &mut State,
    header: [&'static str; 7],
    widths: [Constraint; 7],
    rows: Vec<Row<'a>>,
) {
    state.clamp(rows.len());
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(DIM))
        .row_highlight_style(PICKED)
        .column_spacing(1);
    let mut picked = TableState::default().with_selected(Some(state.selected()));
    frame.render_stateful_widget(table, area, &mut picked);
}

/// A figure, set to the right of its column.
fn right(text: String, style: Style) -> Cell<'static> {
    Cell::from(Line::from(Span::styled(text, style)).right_aligned())
}

fn draw_units(frame: &mut Frame, area: Rect, state: &mut State, snapshot: &Snapshot) {
    let rows = state
        .units(snapshot)
        .into_iter()
        .map(|unit| {
            Row::new(vec![
                Cell::from(unit.name.clone()),
                right(percent(unit.cpu), heat(unit.cpu, 50.0, 150.0)),
                right(bytes(unit.memory), Style::new()),
                right(count(unit.processes), Style::new()),
                right(count(unit.tasks), Style::new()),
                right(bytes(unit.read), Style::new()),
                right(bytes(unit.write), Style::new()),
            ])
        })
        .collect();
    table(
        frame,
        area,
        state,
        [
            "UNIT", "CPU%", "MEMORY", "PROCS", "TASKS", "READ/s", "WRITE/s",
        ],
        [
            Constraint::Min(24),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
        rows,
    );
}

fn draw_processes(frame: &mut Frame, area: Rect, state: &mut State, snapshot: &Snapshot) {
    let rows = state
        .processes(snapshot)
        .into_iter()
        .map(|process| {
            Row::new(vec![
                Cell::from(process.comm.clone()),
                right(process.pid.clone(), DIM),
                Cell::from(process.user.clone()),
                right(percent(process.cpu), heat(process.cpu, 50.0, 150.0)),
                right(bytes(process.rss), Style::new()),
                right(count(process.threads), Style::new()),
                right(figure(process.cpu_seconds, human_duration), Style::new()),
            ])
        })
        .collect();
    table(
        frame,
        area,
        state,
        ["COMMAND", "PID", "USER", "CPU%", "RSS", "THR", "TIME"],
        [
            Constraint::Min(18),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(5),
            Constraint::Length(9),
        ],
        rows,
    );
}

/// The values of a history, one to a column, the latest at the right.
///
/// A step in time, as a key is said to take it: `10s`, `1s`, `2m`.
fn pace(step: f64) -> String {
    let seconds = step.round().max(1.0) as u64;
    if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// A sample stands until the next one is due, `step` seconds on, so a
/// screen wider than the history is long has no holes in it. A column in
/// which nothing was recorded, and nothing was standing, is empty: the
/// collector was not running, or what is drawn was not there.
pub fn columns(history: &[(f64, f64)], until: f64, span: f64, width: usize, step: f64) -> Vec<u64> {
    if width == 0 || span <= 0.0 {
        return Vec::new();
    }
    let from = until - span;
    let each = span / width as f64;
    let mut samples = history.iter().peekable();
    let mut standing: Option<(f64, f64)> = None;
    (0..width)
        .map(|column| {
            let (start, end) = (
                from + each * column as f64,
                from + each * (column + 1) as f64,
            );
            let last = column + 1 == width;
            let mut highest: Option<f64> = None;
            while let Some((at, value)) = samples.peek().copied() {
                // The last column takes the moment the stretch runs up to.
                if *at >= end && !(last && *at <= until) {
                    break;
                }
                if *at >= start {
                    highest = Some(highest.map_or(*value, |h: f64| h.max(*value)));
                }
                standing = Some((*at, *value));
                samples.next();
            }
            let value = highest.or_else(|| {
                standing
                    .filter(|(at, _)| start - at < step * 1.5)
                    .map(|(_, value)| value)
            });
            // The sparkline draws whole numbers, and most of what is
            // drawn here is small: a hundredth is the least that shows.
            (value.unwrap_or(0.0).max(0.0) * 100.0) as u64
        })
        .collect()
}

fn draw_history(frame: &mut Frame, area: Rect, detail: &Detail) {
    let title = if detail.history_of.is_empty() {
        " nothing selected ".to_string()
    } else {
        format!(
            " {}, the {} before ",
            detail.history_of,
            human_duration(detail.history_span)
        )
    };
    let peak = detail
        .history
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::NAN, f64::max);
    let mut block = Block::bordered().title(Span::styled(title, DIM));
    if peak.is_finite() {
        let text = if detail.history_of.ends_with("memory") {
            human_bytes(peak as u64)
        } else {
            format!("{peak:.1}%")
        };
        block =
            block.title(Line::from(Span::styled(format!(" peak {text} "), DIM)).right_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if detail.history.is_empty() {
        let why = if detail.history_of.is_empty() {
            ""
        } else {
            "Not in the store yet: the collector flushes once a minute."
        };
        frame.render_widget(Paragraph::new(Span::styled(why, DIM)), inner);
        return;
    }
    let data = columns(
        &detail.history,
        detail.history_until,
        detail.history_span,
        usize::from(inner.width),
        detail.step,
    );
    frame.render_widget(
        Sparkline::default()
            .data(&data)
            .style(Style::new().fg(Color::Cyan)),
        inner,
    );
}

fn draw_jobs(frame: &mut Frame, area: Rect, state: &mut State, detail: &Detail) {
    let jobs: Vec<&Job> = detail
        .jobs
        .iter()
        .filter(|job| state.wants(&[&job.said]))
        .collect();
    state.clamp(jobs.len());
    let [list, tree] =
        Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);

    let rows: Vec<Row> = jobs
        .iter()
        .map(|job| {
            let failed = if job.failed > 0 {
                right(job.failed.to_string(), BAD)
            } else {
                right("-".into(), DIM)
            };
            let took = if job.running {
                // So far: it has not ended.
                right(format!("{}…", human_duration(job.duration)), GOOD)
            } else {
                right(human_duration(job.duration), Style::new())
            };
            Row::new(vec![
                Cell::from(clock::format(job.started)[11..].to_string()),
                right(job.processes.to_string(), Style::new()),
                failed,
                took,
                right(human_duration(job.cpu), Style::new()),
                Cell::from(job.command.clone()),
                Cell::from(Span::styled(job.unit.clone(), DIM)),
            ])
        })
        .collect();
    table(
        frame,
        list,
        state,
        [
            "STARTED", "PROCS", "FAILED", "TOOK", "CPU", "COMMAND", "UNIT",
        ],
        [
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Min(30),
            Constraint::Min(16),
        ],
        rows,
    );

    let block = Block::bordered().title(Span::styled(" what it ran ", DIM));
    let inner = block.inner(tree);
    frame.render_widget(block, tree);
    let lines: Vec<Line> = match jobs.get(state.selected()) {
        Some(job) => job
            .tree
            .iter()
            .map(|line| {
                if line.contains("  [") {
                    Line::styled(line.clone(), BAD)
                } else {
                    Line::raw(line.clone())
                }
            })
            .collect(),
        None => vec![Line::styled(
            if state.filter.is_empty() {
                format!(
                    "No jobs in the {} before. A job is more than one process.",
                    human_duration(detail.reach)
                )
            } else {
                format!(
                    "No job with \"{}\" in it in the {} before.",
                    state.filter,
                    human_duration(detail.reach)
                )
            },
            DIM,
        )],
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_exits(frame: &mut Frame, area: Rect, state: &mut State, detail: &Detail) {
    let rows: Vec<Row> = detail
        .exits
        .iter()
        .filter(|exit| state.wants(&[&exit.command, &exit.unit, &exit.status, &exit.user]))
        .map(|exit| {
            let style = match exit.level.as_str() {
                "error" => BAD,
                "warning" => WARN,
                "notice" => Style::new().fg(Color::Magenta),
                _ => Style::new(),
            };
            Row::new(vec![
                Cell::from(clock::format(exit.at)[11..].to_string()),
                right(exit.pid.to_string(), DIM),
                Cell::from(Span::styled(exit.status.clone(), style)),
                right(human_duration(exit.elapsed), Style::new()),
                right(human_duration(exit.cpu), Style::new()),
                right(human_bytes(exit.peak_rss), Style::new()),
                Cell::from(exit.command.clone()),
            ])
        })
        .collect();
    if rows.is_empty() && !state.typing {
        // An empty table says nothing of why it is empty.
        let why = match (state.filter.is_empty(), detail.looking) {
            (false, Some(_)) => format!("Looking for \"{}\" …", state.filter),
            (false, None) => format!(
                "No process with \"{}\" in its record ended in the {} before.",
                state.filter,
                human_duration(detail.reach)
            ),
            (true, _) => format!(
                "No process ended in the {} before.",
                human_duration(detail.reach)
            ),
        };
        state.clamp(0);
        frame.render_widget(Paragraph::new(Line::styled(format!(" {why}"), DIM)), area);
        return;
    }
    table(
        frame,
        area,
        state,
        [
            "ENDED", "PID", "STATUS", "TOOK", "CPU", "PEAK RSS", "COMMAND",
        ],
        [
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Min(30),
        ],
        rows,
    );
}

fn draw_keys(frame: &mut Frame, area: Rect, state: &State, detail: &Detail) {
    if let Some(error) = &detail.error {
        frame.render_widget(Paragraph::new(Span::styled(format!(" {error}"), BAD)), area);
        return;
    }
    if let Some(message) = &state.message {
        frame.render_widget(
            Paragraph::new(Span::styled(format!(" {message}"), WARN)),
            area,
        );
        return;
    }
    if let Some(text) = &state.going {
        let line = Line::from(vec![
            Span::styled(" go to ", HEAD),
            Span::styled(format!("{text}▏"), WARN),
            Span::styled(
                "    now   -15m   14:30   2026-09-29 14:30   enter to go, esc to stay",
                DIM,
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    let step = pace(state.step);
    let keys: &[(&str, &str)] = if state.typing {
        &[("enter", "keep"), ("esc", "clear")]
    } else if !state.filter.is_empty() {
        &[
            ("esc", "all of them again"),
            ("/", "only"),
            ("enter", "open"),
            ("m", "its moment"),
            ("←→", &step),
            (",.", "1m"),
            ("<>", "10m"),
            ("t", "go to"),
            ("l", "live"),
            ("tab", "view"),
            ("?", "help"),
        ]
    } else if state.within.is_some() {
        &[
            ("esc", "back to units"),
            ("enter", "open"),
            ("←→", &step),
            (",.", "1m"),
            ("<>", "10m"),
            ("t", "go to"),
            ("l", "live"),
            ("s", "sort"),
            ("/", "only"),
            ("?", "help"),
        ]
    } else {
        &[
            ("←→", &step),
            (",.", "1m"),
            ("<>", "10m"),
            ("[]", "1h"),
            ("t", "go to"),
            ("l", "live"),
            ("-+", "zoom"),
            ("tab", "view"),
            ("enter", "open"),
            ("m", "its moment"),
            ("s", "sort"),
            ("/", "only"),
            ("?", "help"),
            ("q", "quit"),
        ]
    };
    let mut spans = Vec::new();
    for (key, what) in keys {
        spans.push(Span::styled(format!(" {key} "), HEAD));
        spans.push(Span::styled(*what, DIM));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_help(frame: &mut Frame) {
    let text = [
        "Time",
        "  ← →        ten seconds back, forward",
        "  , .        a minute          (or shift ← →)",
        "  < >        ten minutes",
        "  [ ]        an hour",
        "  { }        a day",
        "  home       the first moment in the store",
        "  t          go to a moment: -15m, 14:30, 2026-09-29 14:30",
        "  l, end     now",
        "  - +        a longer stretch of the timeline, a shorter",
        "  m          go to the moment of the selected exit or job",
        "",
        "Under the timeline:  ▲ the moment looked at",
        "                     ! a process ended by a fault   · one was killed",
        "",
        "Views",
        "  tab, 1-4   units, processes, jobs, exits",
        "  ↑ ↓ j k    select a row       pgup pgdn  ten rows",
        "  enter      open it: a unit's processes, or what a process was",
        "  esc        back out of a unit",
        "  g G        the first row, the last",
        "  s          sort by cpu, memory, i/o, name",
        "  a          show slices too: the sums of the units in them",
        "  /          show only what matches",
        "",
        "Now is read from the kernel. Every other moment is read from the",
        "store, which the collector adds to once a minute.",
        "",
        "  q          quit",
    ];
    let area = frame.area();
    let width = 70.min(area.width);
    let height = (text.len() as u16 + 2).min(area.height);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text.map(Line::raw).to_vec())
            .block(Block::bordered().title(Span::styled(" keys ", HEAD))),
        popup,
    );
}

/// A text in lines of at most `width` characters, broken between words
/// where there is a between, and within one where there is not.
pub fn fold(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut length = 0;
    for word in text.split(' ') {
        let mut word: Vec<char> = word.chars().collect();
        if length > 0 && length + 1 + word.len() > width {
            lines.push(std::mem::take(&mut line));
            length = 0;
        }
        // A word longer than a line: a path, or an argument with no
        // spaces in it.
        while word.len() > width {
            if length > 0 {
                lines.push(std::mem::take(&mut line));
            }
            lines.push(word.drain(..width).collect());
            length = 0;
        }
        if length > 0 {
            line.push(' ');
            length += 1;
        }
        length += word.len();
        line.extend(word);
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// What is known of one process, over the rest of the screen.
fn draw_inspected(frame: &mut Frame, title: &str, lines: &[(&'static str, String)]) {
    let area = frame.area();
    let width = area.width.saturating_sub(8).clamp(40.min(area.width), 110);
    let inner = usize::from(width.saturating_sub(18));
    // A command line is as long as it is, and is folded to fit.
    let mut text = Vec::new();
    for (label, value) in lines {
        for (index, part) in fold(value, inner.max(1)).into_iter().enumerate() {
            let label = if index == 0 { *label } else { "" };
            text.push(Line::from(vec![
                Span::styled(format!(" {label:<14} "), DIM),
                Span::raw(part),
            ]));
        }
    }
    let height = (text.len() as u16 + 2).min(area.height);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text).block(
            Block::bordered()
                .title(Span::styled(format!(" {title} "), HEAD))
                .title(Line::from(Span::styled(" any key ", DIM)).right_aligned()),
        ),
        popup,
    );
}

/// What the selected row's history is a history of: the metric, the label
/// that names the row, and what to call it.
pub fn history_of(
    state: &State,
    snapshot: &Snapshot,
) -> Option<(&'static str, &'static str, String, String)> {
    let memory = state.sort == Sort::Memory;
    let what = if memory { "memory" } else { "cpu" };
    match state.tab {
        Tab::Units => {
            let rows = state.units(snapshot);
            let unit = rows.get(state.selected())?;
            let metric = if memory {
                "unit_memory_bytes"
            } else {
                "unit_cpu_pct"
            };
            Some((
                metric,
                "unit",
                unit.name.clone(),
                format!("{} {what}", unit.name),
            ))
        }
        Tab::Processes => {
            let rows = state.processes(snapshot);
            let process = rows.get(state.selected())?;
            let metric = if memory {
                "proc_rss_bytes"
            } else {
                "proc_cpu_pct"
            };
            Some((
                metric,
                "proc",
                process.name.clone(),
                format!("{} {what}", process.name),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::data::tests::batch;
    use super::super::data::Sampled;
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The screen, as the text on it.
    pub(crate) fn screen(state: &mut State, snapshot: &Snapshot, detail: &Detail) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| draw(frame, state, snapshot, detail))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .map(|row| {
                let line: String = row.iter().map(|cell| cell.symbol()).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn snapshot() -> Snapshot {
        Snapshot::read(1_753_000_000.0, &mut Sampled::new(&batch()))
    }

    fn detail() -> Detail {
        Detail {
            now: 1_753_000_300.0,
            range: Some((1_752_000_000.0, 1_753_000_290.0)),
            history: vec![(1_753_000_280.0, 12.0), (1_753_000_290.0, 40.0)],
            history_of: "postgresql.service cpu".into(),
            history_span: 600.0,
            history_until: 1_753_000_290.0,
            step: 10.0,
            timeline_step: 10.0,
            reach: 3600.0,
            ..Detail::default()
        }
    }

    #[test]
    fn the_host_is_on_the_screen_with_its_busiest_unit_first() {
        let mut state = State::new(None, 10.0);
        let text = screen(&mut state, &snapshot(), &detail());
        assert!(text.contains("● LIVE"), "{text}");
        assert!(text.contains("load 2.50 2.00 1.50"), "{text}");
        assert!(text.contains("cpu 12.5%"), "{text}");
        assert!(text.contains("mem 3.7 GiB of 14.9 GiB"), "{text}");
        assert!(text.contains("net ↓1.5 KiB/s"), "{text}");
        let lines: Vec<&str> = text.lines().collect();
        let first = lines.iter().position(|l| l.starts_with("UNIT")).unwrap() + 1;
        assert!(lines[first].starts_with("postgresql.service"), "{text}");
        assert!(lines[first].contains("40.0"), "{text}");
        assert!(lines[first].contains("1.9 GiB"), "{text}");
        assert!(lines[first + 1].starts_with("mark/caddy.service"), "{text}");
        assert!(
            text.contains("postgresql.service cpu, the 10m00s before"),
            "{text}"
        );
        assert!(text.contains("peak 40.0%"), "{text}");
    }

    #[test]
    fn a_moment_gone_back_to_says_when_it_was() {
        let mut state = State::new(Some(1_753_000_000.0), 10.0);
        let text = screen(&mut state, &snapshot(), &detail());
        assert!(!text.contains("LIVE"), "{text}");
        assert!(text.contains(&clock::format(1_753_000_000.0)), "{text}");
        assert!(text.contains("5m00s ago"), "{text}");
    }

    #[test]
    fn a_moment_with_nothing_in_it_says_why() {
        let empty = Snapshot::default();
        let mut state = State::new(Some(1_751_000_000.0), 10.0);
        let text = screen(&mut state, &empty, &detail());
        assert!(text.contains("Before the store began."), "{text}");

        state.at = Some(1_752_500_000.0);
        let text = screen(&mut state, &empty, &detail());
        assert!(text.contains("the collector was not running"), "{text}");

        let nothing = Detail {
            range: None,
            ..detail()
        };
        let text = screen(&mut State::new(None, 10.0), &empty, &nothing);
        assert!(text.contains("The store holds nothing yet."), "{text}");
    }

    #[test]
    fn processes_jobs_and_exits_each_have_a_view() {
        let mut state = State::new(None, 10.0);
        let mut detail = detail();
        detail.jobs = vec![Job {
            started: 1_753_000_100.0,
            duration: 2.5,
            cpu: 1.0,
            processes: 3,
            failed: 1,
            unit: "build.service".into(),
            command: "make all".into(),
            said: "make all\ncc -c a.c".into(),
            running: false,
            tree: vec![
                "make all  2.5s, cpu 500ms".into(),
                "└─ cc -c a.c  1.0s, cpu 500ms  [exited 1]".into(),
            ],
        }];
        detail.exits = vec![Exit {
            at: 1_753_000_200.0,
            pid: 4242,
            status: "SIGSEGV".into(),
            level: "error".into(),
            elapsed: 0.394,
            cpu: 0.001,
            peak_rss: 2_400_000,
            command: "sleep 100".into(),
            ..Exit::default()
        }];

        state.tab = Tab::Processes;
        let text = screen(&mut state, &snapshot(), &detail);
        assert!(text.contains("COMMAND"), "{text}");
        let postgres = text.lines().find(|l| l.starts_with("postgres")).unwrap();
        assert!(
            postgres.contains("100") && postgres.contains("38.0"),
            "{text}"
        );

        state.tab = Tab::Jobs;
        let text = screen(&mut state, &snapshot(), &detail);
        assert!(text.contains("make all"), "{text}");
        assert!(
            text.contains("└─ cc -c a.c  1.0s, cpu 500ms  [exited 1]"),
            "{text}"
        );

        state.tab = Tab::Exits;
        let text = screen(&mut state, &snapshot(), &detail);
        let exit = text.lines().find(|l| l.contains("SIGSEGV")).unwrap();
        assert!(exit.contains("4242") && exit.contains("394ms"), "{text}");
        assert!(exit.contains("sleep 100"), "{text}");

        // An exit is picked out by how it ended, as well as by what ran.
        state.filter = "segv".into();
        let text = screen(&mut state, &snapshot(), &detail);
        assert!(text.contains("sleep 100"), "{text}");
        // What does not match is not shown.
        state.filter = "nothing-like-it".into();
        let text = screen(&mut state, &snapshot(), &detail);
        assert!(!text.contains("SIGSEGV"), "{text}");
        // And says so, and what is on the keys for getting back.
        assert!(
            text.contains(
                "No process with \"nothing-like-it\" in its record ended in the 1h00m before"
            ),
            "{text}"
        );
        assert!(text.contains("esc all of them again"), "{text}");
        // While the records are still being read, that is what it says.
        detail.looking = Some(1_753_000_100.0);
        let text = screen(&mut state, &snapshot(), &detail);
        assert!(text.contains("Looking for \"nothing-like-it\""), "{text}");
        assert!(text.contains("read back to"), "{text}");

        // A job is found by anything that ran in it.
        detail.looking = None;
        state.tab = Tab::Jobs;
        state.filter = "cc -c".into();
        let text = screen(&mut state, &snapshot(), &detail);
        assert!(text.contains("make all"), "{text}");
        state.filter = "nothing-like-it".into();
        let text = screen(&mut state, &snapshot(), &detail);
        assert!(
            text.contains("No job with \"nothing-like-it\" in it in the 1h00m before."),
            "{text}"
        );
    }

    #[test]
    fn what_is_known_of_a_process_is_shown_over_the_rest() {
        let mut state = State::new(None, 10.0);
        state.inspecting = true;
        let mut detail = detail();
        detail.inspected = Some((
            "rustc[4242]".into(),
            vec![
                (
                    "ran",
                    format!("rustc --crate-name acct {}", "--cfg feature ".repeat(12)),
                ),
                ("in", "mark/app-term.scope".into()),
                ("ended", "exited 1, after 2.5s".into()),
            ],
        ));
        let text = screen(&mut state, &snapshot(), &detail);
        assert!(text.contains(" rustc[4242] "), "{text}");
        assert!(
            text.contains("ran            rustc --crate-name acct"),
            "{text}"
        );
        // Folded, and not cut: all of the command line is there.
        assert_eq!(text.matches("--cfg").count(), 12, "{text}");
        assert_eq!(text.matches("feature").count(), 12, "{text}");
        assert!(
            text.contains("ended          exited 1, after 2.5s"),
            "{text}"
        );
    }

    #[test]
    fn what_went_wrong_is_marked_under_the_moment_it_happened() {
        use Mark::*;
        let incidents = [
            Incident {
                at: 105.0,
                error: false,
            },
            Incident {
                at: 131.0,
                error: true,
            },
            // A kill and a fault in the same column: the fault shows.
            Incident {
                at: 138.0,
                error: false,
            },
            Incident {
                at: 171.0,
                error: false,
            },
            // Outside the stretch.
            Incident {
                at: 50.0,
                error: true,
            },
            Incident {
                at: 250.0,
                error: true,
            },
        ];
        assert_eq!(
            marks(10, 100.0, 200.0, 155.0, &incidents),
            [Killed, Nothing, Nothing, Fault, Nothing, Here, Nothing, Killed, Nothing, Nothing]
        );
        // Looking at the moment something went wrong.
        let at = |cursor| marks(10, 100.0, 200.0, cursor, &incidents);
        assert_eq!(at(131.0)[3], HereFault);
        assert_eq!(at(171.0)[7], HereKilled);
        // Now is at the right end.
        assert_eq!(at(200.0)[9], Here);
        // A moment outside the stretch is not on it.
        assert!(at(999.0).iter().all(|mark| !matches!(mark, Here)));
        assert!(marks(0, 100.0, 200.0, 150.0, &incidents).is_empty());
    }

    #[test]
    fn the_timeline_is_in_the_header_with_the_stretch_it_shows() {
        let mut state = State::new(Some(1_753_000_000.0), 10.0);
        let mut detail = detail();
        detail.window = (1_752_998_200.0, 1_753_001_800.0);
        detail.timeline = (0..360)
            .map(|n| (1_752_998_200.0 + 10.0 * f64::from(n), f64::from(n % 50)))
            .collect();
        detail.timeline_step = 10.0;
        detail.incidents = vec![Incident {
            at: 1_752_999_100.0,
            error: true,
        }];
        let text = screen(&mut state, &snapshot(), &detail);
        let lines: Vec<&str> = text.lines().collect();
        // The moment looked at is in the middle of the stretch, and what
        // went wrong a quarter of the way along it.
        let marked = lines[4];
        let cursor = marked.chars().position(|c| c == '▲').unwrap();
        let fault = marked.chars().position(|c| c == '!').unwrap();
        assert!((54..=56).contains(&cursor), "{cursor}\n{text}");
        assert!((27..=29).contains(&fault), "{fault}\n{text}");
        assert!(lines[3].contains('▁') || lines[3].contains('▂'), "{text}");
        assert!(lines[5].contains(" cpu over 1h00m, up to 49.0% "), "{text}");
        assert!(text.contains("56°C 20W"), "{text}");
    }

    #[test]
    fn a_job_that_is_running_says_so() {
        let mut state = State::new(None, 10.0);
        state.tab = Tab::Jobs;
        let mut detail = detail();
        detail.jobs = vec![Job {
            started: 1_753_000_100.0,
            duration: 95.0,
            processes: 3,
            command: "cargo build".into(),
            tree: vec!["cargo build  1m35s, cpu 4m02s".into()],
            running: true,
            ..Job::default()
        }];
        let text = screen(&mut state, &snapshot(), &detail);
        let job = text
            .lines()
            .find(|l| l.contains("cargo build") && l.contains("1m35s…"));
        assert!(job.is_some(), "{text}");
    }

    #[test]
    fn a_text_is_folded_between_its_words() {
        assert_eq!(fold("cc -c a.c", 40), ["cc -c a.c"]);
        assert_eq!(fold("cc -c a.c -o a.o", 9), ["cc -c a.c", "-o a.o"]);
        assert_eq!(fold("one two three", 7), ["one two", "three"]);
        // A word longer than a line is broken where the line ends.
        assert_eq!(
            fold("ld /usr/lib/very/long/path.o -o x", 10),
            ["ld", "/usr/lib/v", "ery/long/p", "ath.o -o x"]
        );
        assert_eq!(fold("", 10), [""]);
        assert_eq!(fold("héllo wörld", 5), ["héllo", "wörld"]);
    }

    #[test]
    fn a_unit_gone_into_and_a_moment_being_typed_are_said() {
        let mut state = State::new(None, 10.0);
        state.enter("postgresql.service".into());
        let text = screen(&mut state, &snapshot(), &detail());
        assert!(text.contains("in postgresql.service"), "{text}");
        assert!(text.contains("esc back to units"), "{text}");
        assert!(!text.contains("caddy"), "{text}");

        state.going = Some("-15".into());
        let text = screen(&mut state, &snapshot(), &detail());
        assert!(text.contains("go to -15▏"), "{text}");

        state.going = None;
        state.message = Some("\"soon\" is not a time".into());
        let text = screen(&mut state, &snapshot(), &detail());
        assert!(text.contains("\"soon\" is not a time"), "{text}");
    }

    #[test]
    fn trouble_reading_the_store_is_said_and_the_screen_is_still_drawn() {
        let mut detail = detail();
        detail.error = Some("database is locked".into());
        let text = screen(&mut State::new(None, 10.0), &snapshot(), &detail);
        assert!(text.contains("database is locked"), "{text}");
        assert!(text.contains("postgresql.service"), "{text}");
    }

    #[test]
    fn a_history_is_laid_out_in_time_with_the_latest_at_the_right() {
        // A sample every ten seconds, drawn a column to five.
        let history = [(100.0, 1.0), (110.0, 2.5), (190.0, 3.0), (200.0, 4.0)];
        let columns = columns(&history, 200.0, 100.0, 20, 10.0);
        assert_eq!(
            columns,
            [
                // Each stands until the next is due.
                100, 100, 250, 250, 250, //
                // None came: the collector was not running.
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
                300, 400
            ]
        );
    }

    #[test]
    fn a_history_longer_than_the_screen_is_wide_shows_its_peaks() {
        let history: Vec<(f64, f64)> = (0..100)
            .map(|n| (100.0 + f64::from(n), if n == 37 { 9.0 } else { 1.0 }))
            .collect();
        let columns = columns(&history, 200.0, 100.0, 10, 1.0);
        assert_eq!(columns, [100, 100, 100, 900, 100, 100, 100, 100, 100, 100]);
    }

    #[test]
    fn what_is_outside_the_stretch_is_not_drawn() {
        // Before it, and standing no longer.
        assert_eq!(columns(&[(50.0, 9.0)], 200.0, 100.0, 2, 10.0), [0, 0]);
        // Just before it, and standing still.
        assert_eq!(
            columns(&[(95.0, 9.0)], 200.0, 100.0, 4, 10.0),
            [900, 0, 0, 0]
        );
        assert!(columns(&[(150.0, 1.0)], 200.0, 100.0, 0, 10.0).is_empty());
        assert_eq!(columns(&[], 200.0, 100.0, 3, 10.0), [0, 0, 0]);
    }

    #[test]
    fn the_selected_row_names_the_history_to_read() {
        let snapshot = snapshot();
        let mut state = State::new(None, 10.0);
        let (metric, key, want, title) = history_of(&state, &snapshot).unwrap();
        assert_eq!((metric, key), ("unit_cpu_pct", "unit"));
        assert_eq!(want, "postgresql.service");
        assert_eq!(title, "postgresql.service cpu");

        state.sort = Sort::Memory;
        state.tab = Tab::Processes;
        let (metric, key, want, _) = history_of(&state, &snapshot).unwrap();
        assert_eq!(
            (metric, key, want.as_str()),
            ("proc_rss_bytes", "proc", "postgres[100]")
        );

        state.tab = Tab::Jobs;
        assert!(history_of(&state, &snapshot).is_none());
    }
}
