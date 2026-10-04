//! What is being looked at: which moment, which view, which row.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::data::{Process, Snapshot, Unit};
use crate::cgroup::is_sum;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Units,
    Processes,
    Jobs,
    Exits,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Units, Tab::Processes, Tab::Jobs, Tab::Exits];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Units => "Units",
            Tab::Processes => "Processes",
            Tab::Jobs => "Jobs",
            Tab::Exits => "Exits",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Cpu,
    Memory,
    Io,
    Name,
}

impl Sort {
    pub fn title(self) -> &'static str {
        match self {
            Sort::Cpu => "cpu",
            Sort::Memory => "memory",
            Sort::Io => "i/o",
            Sort::Name => "name",
        }
    }

    fn next(self) -> Self {
        match self {
            Sort::Cpu => Sort::Memory,
            Sort::Memory => Sort::Io,
            Sort::Io => Sort::Name,
            Sort::Name => Sort::Cpu,
        }
    }
}

/// What a key changed, which is what has to be done about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changed {
    Nothing,
    /// How it is drawn.
    View,
    /// Which moment, or which part of it: there is reading to do.
    Moment,
    /// The selected row is to be opened: a unit into its processes, a
    /// process or an exit into what is known of it.
    Open,
    /// The moment of the selected row is to be gone to: when a process
    /// ended, when a job began.
    Go,
}

/// The stretches of time the timeline can show.
pub const WINDOWS: [f64; 5] = [600.0, 3600.0, 6.0 * 3600.0, 86_400.0, 7.0 * 86_400.0];

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub tab: Tab,
    pub sort: Sort,
    /// The moment looked at, in epoch seconds; `None` is now.
    pub at: Option<f64>,
    /// The row selected in each view.
    selected: [usize; 4],
    pub filter: String,
    /// Whether keys are going into the filter.
    pub typing: bool,
    pub help: bool,
    pub quit: bool,
    /// Whether the units that are made of other units are shown.
    pub sums: bool,
    /// The unit whose processes are the only ones shown.
    pub within: Option<String>,
    /// A moment being typed, to go to.
    pub going: Option<String>,
    /// Whether what is known of the selected row is on the screen.
    pub inspecting: bool,
    /// Something to say, until the next key.
    pub message: Option<String>,
    /// Which of the stretches the timeline shows.
    window: usize,
    /// Seconds between the store's samples: the smallest step in time.
    pub step: f64,
}

impl State {
    pub fn new(at: Option<f64>, step: f64) -> Self {
        Self {
            tab: Tab::Units,
            sort: Sort::Cpu,
            at,
            selected: [0; 4],
            filter: String::new(),
            typing: false,
            help: false,
            quit: false,
            sums: false,
            within: None,
            going: None,
            inspecting: false,
            message: None,
            window: 1,
            step: step.max(1.0),
        }
    }

    /// How long a stretch the timeline shows, in seconds.
    pub fn window(&self) -> f64 {
        WINDOWS[self.window]
    }

    /// Go to a moment, as near as the store holds one. `range` is the
    /// first and last moments it holds.
    pub fn go_to(&mut self, at: f64, range: Option<(f64, f64)>) {
        let Some((first, last)) = range else {
            return;
        };
        let at = (at / self.step).floor() * self.step;
        self.at = if at > last { None } else { Some(at.max(first)) };
        // What ended then is what ended last, up to then.
        self.selected[self.tab.index()] = 0;
    }

    pub fn is_live(&self) -> bool {
        self.at.is_none()
    }

    pub fn selected(&self) -> usize {
        self.selected[self.tab.index()]
    }

    /// Keep the selection on a row there is.
    pub fn clamp(&mut self, rows: usize) {
        let selected = &mut self.selected[self.tab.index()];
        *selected = (*selected).min(rows.saturating_sub(1));
    }

    fn select(&mut self, by: isize) {
        let selected = &mut self.selected[self.tab.index()];
        *selected = selected.saturating_add_signed(by);
    }

    /// Move through time by `seconds`. `range` is the first and last
    /// moments the store holds.
    ///
    /// Going back from now lands on what the store holds, not on the
    /// moment that many seconds ago: the collector flushes once a minute,
    /// and the last minute is not in the store yet. Going forward past the
    /// last moment stored is going back to now.
    fn shift(&mut self, seconds: f64, range: Option<(f64, f64)>) -> Changed {
        let Some((first, last)) = range else {
            return Changed::Nothing;
        };
        // Going back from now lands on what the store holds.
        if self.at.is_none() && seconds < 0.0 {
            self.at = Some(last);
            return Changed::Moment;
        }
        let from = self.at.unwrap_or(last + self.step);
        let to = ((from + seconds) / self.step).floor() * self.step;
        let moved = if to > last { None } else { Some(to.max(first)) };
        if moved == self.at {
            return Changed::Nothing;
        }
        self.at = moved;
        Changed::Moment
    }

    /// Show the processes of one unit, and nothing else.
    pub fn enter(&mut self, unit: String) {
        self.within = Some(unit);
        // What picked the unit out is not what picks its processes out.
        self.filter.clear();
        self.tab = Tab::Processes;
        self.selected[Tab::Processes.index()] = 0;
    }

    /// Go to a moment that was typed. `now` is epoch seconds.
    fn go(&mut self, text: &str, now: f64, range: Option<(f64, f64)>) -> Changed {
        let to = match crate::clock::parse(text, now) {
            Ok(to) => to,
            Err(error) => {
                self.message = Some(error.to_string());
                return Changed::View;
            }
        };
        let Some((first, last)) = range else {
            self.message = Some("The store holds nothing to go to.".into());
            return Changed::View;
        };
        let at = (to / self.step).floor() * self.step;
        self.at = if at > last { None } else { Some(at) };
        if at < first {
            self.message = Some(format!(
                "The store begins at {}.",
                crate::clock::format(first)
            ));
            self.at = Some(first);
        }
        Changed::Moment
    }

    /// `now` is epoch seconds; `range` is the first and last moments the
    /// store holds.
    pub fn key(&mut self, key: KeyEvent, now: f64, range: Option<(f64, f64)>) -> Changed {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return Changed::View;
        }
        self.message = None;
        if let Some(text) = &mut self.going {
            match key.code {
                KeyCode::Enter => {
                    let text = std::mem::take(text);
                    self.going = None;
                    if text.trim().is_empty() {
                        return Changed::View;
                    }
                    return self.go(&text, now, range);
                }
                KeyCode::Esc => self.going = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) => text.push(c),
                _ => return Changed::Nothing,
            }
            return Changed::View;
        }
        if self.typing {
            match key.code {
                // What is wanted has been said: there is reading to do,
                // for what matches and is further back than what is
                // on the screen.
                KeyCode::Enter => {
                    self.typing = false;
                    return Changed::Moment;
                }
                KeyCode::Esc => {
                    self.typing = false;
                    self.filter.clear();
                    return Changed::Moment;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => return Changed::Nothing,
            }
            return Changed::View;
        }
        if self.help || self.inspecting {
            self.help = false;
            self.inspecting = false;
            return Changed::View;
        }

        let shifted = key.modifiers.contains(KeyModifiers::SHIFT);
        let (minute, hour) = (60.0, 3600.0);
        match key.code {
            // Back from what was done last: from what is looked for, then
            // out of a unit, and only then out of the program.
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                return Changed::Moment;
            }
            KeyCode::Esc | KeyCode::Backspace if self.within.is_some() => {
                self.within = None;
                self.tab = Tab::Units;
                return Changed::Moment;
            }
            KeyCode::Backspace => return Changed::Nothing,
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') | KeyCode::Char('h') => self.help = true,
            KeyCode::Enter => return Changed::Open,
            KeyCode::Char('m') => return Changed::Go,
            KeyCode::Char('-') | KeyCode::Char('_') => {
                if self.window + 1 == WINDOWS.len() {
                    return Changed::Nothing;
                }
                self.window += 1;
                return Changed::Moment;
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                if self.window == 0 {
                    return Changed::Nothing;
                }
                self.window -= 1;
                return Changed::Moment;
            }
            KeyCode::Char('t') => {
                self.going = Some(String::new());
                return Changed::View;
            }

            KeyCode::Left if shifted => return self.shift(-minute, range),
            KeyCode::Right if shifted => return self.shift(minute, range),
            KeyCode::Left => return self.shift(-self.step, range),
            KeyCode::Right => return self.shift(self.step, range),
            KeyCode::Char(',') => return self.shift(-minute, range),
            KeyCode::Char('.') => return self.shift(minute, range),
            KeyCode::Char('<') => return self.shift(-10.0 * minute, range),
            KeyCode::Char('>') => return self.shift(10.0 * minute, range),
            KeyCode::Char('[') => return self.shift(-hour, range),
            KeyCode::Char(']') => return self.shift(hour, range),
            KeyCode::Char('{') => return self.shift(-24.0 * hour, range),
            KeyCode::Char('}') => return self.shift(24.0 * hour, range),
            KeyCode::Home => {
                let first = range.map(|(first, _)| first);
                if first.is_none() || first == self.at {
                    return Changed::Nothing;
                }
                self.at = first;
                return Changed::Moment;
            }
            KeyCode::End | KeyCode::Char('l') => {
                if self.at.is_none() {
                    return Changed::Nothing;
                }
                self.at = None;
                return Changed::Moment;
            }

            KeyCode::Tab | KeyCode::BackTab => {
                let count = Tab::ALL.len();
                let step = if key.code == KeyCode::Tab {
                    1
                } else {
                    count - 1
                };
                self.tab = Tab::ALL[(self.tab.index() + step) % count];
                return Changed::Moment;
            }
            KeyCode::Char(c @ '1'..='4') => {
                self.tab = Tab::ALL[c as usize - '1' as usize];
                return Changed::Moment;
            }

            KeyCode::Down | KeyCode::Char('j') => self.select(1),
            KeyCode::Up | KeyCode::Char('k') => self.select(-1),
            KeyCode::PageDown => self.select(10),
            KeyCode::PageUp => self.select(-10),
            KeyCode::Char('g') => self.selected[self.tab.index()] = 0,
            KeyCode::Char('G') => self.selected[self.tab.index()] = usize::MAX,
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.selected[self.tab.index()] = 0;
            }
            KeyCode::Char('/') => {
                self.typing = true;
                self.filter.clear();
            }
            KeyCode::Char('a') => {
                self.sums = !self.sums;
                self.selected[self.tab.index()] = 0;
            }
            _ => return Changed::Nothing,
        }
        // Another row selected has another history to read.
        Changed::Moment
    }

    fn wanted(&self, texts: &[&str]) -> bool {
        let filter = self.filter.to_lowercase();
        Self::matches(&filter, texts)
    }

    fn matches(filter: &str, texts: &[&str]) -> bool {
        filter.is_empty()
            || texts
                .iter()
                .any(|text| text.to_lowercase().contains(filter))
    }

    /// The units to show, in the order to show them.
    pub fn units<'a>(&self, snapshot: &'a Snapshot) -> Vec<&'a Unit> {
        let filter = self.filter.to_lowercase();
        let mut rows: Vec<&Unit> = snapshot
            .units
            .iter()
            .filter(|unit| self.sums || !is_sum(&unit.name))
            .filter(|unit| Self::matches(&filter, &[&unit.name]))
            .collect();
        let sort = self.sort;
        order(&mut rows, sort, |unit| match sort {
            Sort::Cpu => unit.cpu,
            Sort::Memory => unit.memory,
            Sort::Io => total(unit.read, unit.write),
            Sort::Name => None,
        });
        rows
    }

    /// The processes to show, in the order to show them.
    pub fn processes<'a>(&self, snapshot: &'a Snapshot) -> Vec<&'a Process> {
        let filter = self.filter.to_lowercase();
        let mut rows: Vec<&Process> = snapshot
            .processes
            .iter()
            .filter(|p| self.within.as_ref().is_none_or(|unit| *unit == p.unit))
            .filter(|p| Self::matches(&filter, &[&p.name, &p.user]))
            .collect();
        let sort = self.sort;
        order(&mut rows, sort, |process| match sort {
            Sort::Cpu => process.cpu,
            Sort::Memory => process.rss,
            Sort::Io => total(process.read, process.write),
            Sort::Name => None,
        });
        rows
    }

    /// Whether a job or an exit is wanted: by what it ran, where, as
    /// whom, or how it ended.
    pub fn wants(&self, about: &[&str]) -> bool {
        self.wanted(about)
    }

    /// Whether only some of what there is, is wanted.
    pub fn is_looking(&self) -> bool {
        !self.filter.is_empty()
    }
}

fn total(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

/// The largest first, and what has no figure last. The rows come in by
/// name, and a stable sort leaves equals that way: the order on the screen
/// does not change when nothing has.
fn order<T>(rows: &mut [&T], sort: Sort, figure: impl Fn(&T) -> Option<f64>) {
    if sort == Sort::Name {
        return;
    }
    rows.sort_by(|a, b| match (figure(a), figure(b)) {
        (Some(a), Some(b)) => b.total_cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
}

#[cfg(test)]
mod tests {
    use super::super::data::tests::batch;
    use super::super::data::Sampled;
    use super::*;

    const RANGE: Option<(f64, f64)> = Some((1000.0, 5000.0));
    const NOW: f64 = 5030.0;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(state: &mut State, text: &str) -> Changed {
        text.chars()
            .map(|c| state.key(key(KeyCode::Char(c)), NOW, RANGE))
            .last()
            .unwrap_or(Changed::Nothing)
    }

    #[test]
    fn going_back_from_now_lands_on_the_last_moment_stored() {
        let mut state = State::new(None, 10.0);
        assert!(state.is_live());
        assert_eq!(state.key(key(KeyCode::Left), NOW, RANGE), Changed::Moment);
        assert_eq!(state.at, Some(5000.0));
        state.key(key(KeyCode::Left), NOW, RANGE);
        assert_eq!(state.at, Some(4990.0));
        press(&mut state, ",");
        assert_eq!(state.at, Some(4930.0));
        press(&mut state, "<");
        assert_eq!(state.at, Some(4330.0));
        press(&mut state, "[");
        assert_eq!(state.at, Some(1000.0), "no further than the first");
        assert_eq!(press(&mut state, "["), Changed::Nothing);
    }

    #[test]
    fn going_forward_past_the_last_moment_stored_is_going_back_to_now() {
        let mut state = State::new(Some(4990.0), 10.0);
        state.key(key(KeyCode::Right), NOW, RANGE);
        assert_eq!(state.at, Some(5000.0));
        assert_eq!(state.key(key(KeyCode::Right), NOW, RANGE), Changed::Moment);
        assert!(state.is_live());
        assert_eq!(state.key(key(KeyCode::Right), NOW, RANGE), Changed::Nothing);

        state.at = Some(2000.0);
        press(&mut state, "l");
        assert!(state.is_live());
        state.key(key(KeyCode::Home), NOW, RANGE);
        assert_eq!(state.at, Some(1000.0));
        state.key(key(KeyCode::End), NOW, RANGE);
        assert!(state.is_live());
    }

    #[test]
    fn a_moment_is_one_the_store_sampled() {
        let mut state = State::new(Some(4995.0), 10.0);
        state.key(key(KeyCode::Left), NOW, RANGE);
        assert_eq!(state.at, Some(4980.0));
        let shifted = KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT);
        state.key(shifted, NOW, RANGE);
        assert_eq!(state.at, Some(4920.0));
    }

    #[test]
    fn with_nothing_stored_there_is_only_now() {
        let mut state = State::new(None, 10.0);
        assert_eq!(state.key(key(KeyCode::Left), NOW, None), Changed::Nothing);
        assert_eq!(state.key(key(KeyCode::Home), NOW, None), Changed::Nothing);
        assert!(state.is_live());
    }

    #[test]
    fn views_are_stepped_through_and_jumped_to() {
        let mut state = State::new(None, 10.0);
        state.key(key(KeyCode::Tab), NOW, RANGE);
        assert_eq!(state.tab, Tab::Processes);
        press(&mut state, "4");
        assert_eq!(state.tab, Tab::Exits);
        state.key(key(KeyCode::Tab), NOW, RANGE);
        assert_eq!(state.tab, Tab::Units);
        state.key(key(KeyCode::BackTab), NOW, RANGE);
        assert_eq!(state.tab, Tab::Exits);
    }

    #[test]
    fn each_view_keeps_its_own_selection() {
        let mut state = State::new(None, 10.0);
        press(&mut state, "jjj");
        assert_eq!(state.selected(), 3);
        press(&mut state, "2");
        assert_eq!(state.selected(), 0);
        press(&mut state, "j1");
        assert_eq!(state.selected(), 3);
        press(&mut state, "kkkkk");
        assert_eq!(state.selected(), 0);
        press(&mut state, "G");
        state.clamp(7);
        assert_eq!(state.selected(), 6);
        state.clamp(0);
        assert_eq!(state.selected(), 0);
    }

    #[test]
    fn rows_are_put_in_order_and_picked_out() {
        let snapshot = Snapshot::read(0.0, &mut Sampled::new(&batch()));
        let mut state = State::new(None, 10.0);
        let names = |state: &State| -> Vec<String> {
            state
                .units(&snapshot)
                .iter()
                .map(|unit| unit.name.clone())
                .collect()
        };
        // By CPU, the busiest first, and what has no figure yet last.
        assert_eq!(
            names(&state),
            ["postgresql.service", "mark/caddy.service", "new.service"]
        );
        press(&mut state, "s");
        assert_eq!(state.sort, Sort::Memory);
        assert_eq!(names(&state)[0], "postgresql.service");
        press(&mut state, "ss");
        assert_eq!(state.sort, Sort::Name);
        assert_eq!(
            names(&state),
            ["mark/caddy.service", "new.service", "postgresql.service"]
        );

        press(&mut state, "/CADDY");
        assert!(state.typing);
        assert_eq!(names(&state), ["mark/caddy.service"]);
        // While typing, a key is a letter and not a command.
        press(&mut state, "q");
        assert!(!state.quit);
        assert_eq!(state.filter, "CADDYq");
        state.key(key(KeyCode::Backspace), NOW, RANGE);
        state.key(key(KeyCode::Enter), NOW, RANGE);
        assert!(!state.typing);
        assert_eq!(names(&state), ["mark/caddy.service"]);

        press(&mut state, "2");
        let processes = state.processes(&snapshot);
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].name, "caddy[200]");
        // By the user as well as by the name.
        press(&mut state, "/postgres");
        state.key(key(KeyCode::Enter), NOW, RANGE);
        assert_eq!(state.processes(&snapshot)[0].user, "postgres");

        press(&mut state, "/");
        state.key(key(KeyCode::Esc), NOW, RANGE);
        assert_eq!(state.filter, "");
        assert_eq!(state.processes(&snapshot).len(), 2);
    }

    #[test]
    fn escape_goes_back_from_what_is_looked_for_before_it_goes_out() {
        let mut state = State::new(None, 10.0);
        state.enter("db.service".into());
        press(&mut state, "/");
        press(&mut state, "post");
        state.key(key(KeyCode::Enter), NOW, RANGE);
        assert_eq!(state.filter, "post");
        assert!(!state.typing);

        // The filter, then the unit, then the program.
        assert_eq!(state.key(key(KeyCode::Esc), NOW, RANGE), Changed::Moment);
        assert_eq!(state.filter, "");
        assert!(state.within.is_some());
        assert!(!state.quit);
        state.key(key(KeyCode::Esc), NOW, RANGE);
        assert!(state.within.is_none());
        assert!(!state.quit);
        state.key(key(KeyCode::Esc), NOW, RANGE);
        assert!(state.quit);
    }

    #[test]
    fn units_made_of_other_units_are_shown_when_asked_for() {
        let mut snapshot = Snapshot::read(0.0, &mut Sampled::new(&batch()));
        for name in ["user.slice", "mark/app.slice", "user@1000.service"] {
            snapshot.units.push(Unit {
                name: name.into(),
                cpu: Some(99.0),
                ..Unit::default()
            });
        }
        let mut state = State::new(None, 10.0);
        let units = state.units(&snapshot);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].name, "postgresql.service");

        press(&mut state, "a");
        assert_eq!(state.units(&snapshot).len(), 6);
        press(&mut state, "a");
        assert_eq!(state.units(&snapshot).len(), 3);
    }

    #[test]
    fn a_unit_is_gone_into_and_come_out_of() {
        let snapshot = Snapshot::read(0.0, &mut Sampled::new(&batch()));
        let mut state = State::new(None, 10.0);
        // The row is opened by whoever knows what is in it.
        assert_eq!(state.key(key(KeyCode::Enter), NOW, RANGE), Changed::Open);
        state.filter = "postgresql".into();
        state.enter("postgresql.service".into());
        assert_eq!(state.tab, Tab::Processes);
        assert_eq!(state.filter, "");
        let inside = state.processes(&snapshot);
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].name, "postgres[100]");

        // Escape leaves the unit, and not the program.
        assert_eq!(state.key(key(KeyCode::Esc), NOW, RANGE), Changed::Moment);
        assert!(!state.quit);
        assert_eq!(state.tab, Tab::Units);
        assert_eq!(state.within, None);
        press(&mut state, "2");
        assert_eq!(state.processes(&snapshot).len(), 2);
        state.key(key(KeyCode::Esc), NOW, RANGE);
        assert!(state.quit);
    }

    #[test]
    fn a_moment_is_gone_to_by_typing_it() {
        let mut state = State::new(None, 10.0);
        press(&mut state, "t");
        assert_eq!(state.going.as_deref(), Some(""));
        // While typing, a key is a letter and not a command.
        press(&mut state, "-10mq");
        state.key(key(KeyCode::Backspace), NOW, RANGE);
        assert_eq!(state.going.as_deref(), Some("-10m"));
        assert_eq!(state.key(key(KeyCode::Enter), NOW, RANGE), Changed::Moment);
        assert_eq!(state.going, None);
        assert_eq!(state.at, Some(4430.0));

        // What is after the last moment stored is now.
        press(&mut state, "t-5s");
        state.key(key(KeyCode::Enter), NOW, RANGE);
        assert!(state.is_live());

        // What is before the first is the first, and says so.
        press(&mut state, "t-2h");
        state.key(key(KeyCode::Enter), NOW, RANGE);
        assert_eq!(state.at, Some(1000.0));
        assert!(state
            .message
            .as_deref()
            .unwrap()
            .starts_with("The store begins at"));
        // The next key puts what was said away.
        press(&mut state, "j");
        assert_eq!(state.message, None);

        // What is not a time is refused, and nothing moves.
        press(&mut state, "tyesterday");
        assert_eq!(state.key(key(KeyCode::Enter), NOW, RANGE), Changed::View);
        assert_eq!(state.at, Some(1000.0));
        assert!(state.message.as_deref().unwrap().contains("is not a time"));

        press(&mut state, "t12");
        state.key(key(KeyCode::Esc), NOW, RANGE);
        assert_eq!(state.going, None);
        assert_eq!(state.at, Some(1000.0));
    }

    #[test]
    fn the_timeline_is_drawn_out_and_drawn_in() {
        let mut state = State::new(None, 10.0);
        assert_eq!(state.window(), 3600.0);
        assert_eq!(press(&mut state, "-"), Changed::Moment);
        assert_eq!(state.window(), 6.0 * 3600.0);
        press(&mut state, "---");
        assert_eq!(state.window(), 7.0 * 86_400.0);
        assert_eq!(press(&mut state, "-"), Changed::Nothing);
        press(&mut state, "++++");
        assert_eq!(state.window(), 600.0);
        assert_eq!(press(&mut state, "="), Changed::Nothing);
    }

    #[test]
    fn the_moment_of_a_row_is_gone_to() {
        let mut state = State::new(None, 10.0);
        press(&mut state, "4jjj");
        assert_eq!(press(&mut state, "m"), Changed::Go);
        // By whoever knows when the row was.
        state.go_to(3217.4, RANGE);
        assert_eq!(state.at, Some(3210.0));
        assert_eq!(state.selected(), 0);
        state.go_to(9999.0, RANGE);
        assert!(state.is_live());
        state.go_to(5.0, RANGE);
        assert_eq!(state.at, Some(1000.0));
        state.go_to(2000.0, None);
        assert_eq!(state.at, Some(1000.0));
    }

    #[test]
    fn leaving() {
        let mut state = State::new(None, 10.0);
        press(&mut state, "?");
        assert!(state.help);
        // Any key puts the help away, and does nothing else.
        press(&mut state, "q");
        assert!(!state.help && !state.quit);
        press(&mut state, "q");
        assert!(state.quit);

        let mut state = State::new(None, 10.0);
        state.typing = true;
        state.key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            NOW,
            RANGE,
        );
        assert!(state.quit);
    }
}
