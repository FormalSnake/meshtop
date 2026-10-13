use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::config::{Config, Host};
use crate::model::Snapshot;
use crate::source::Update;
use crate::topo::{self, Topology};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Overview,
    Map,
    Agents,
    Host,
}

/// What a click on a screen region does.
#[derive(Clone, Copy)]
pub enum Hit {
    Tab(View),
    Host(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Cpu,
    Mem,
}

pub enum Status {
    Connecting,
    Up,
    Down(String),
}

pub struct HostState {
    pub cfg: Host,
    pub local: bool,
    pub status: Status,
    pub snap: Option<Snapshot>,
    pub seen: Option<Instant>,
    pub cpu: VecDeque<u64>,
    pub mem: VecDeque<u64>,
    pub rx: VecDeque<u64>,
    pub tx: VecDeque<u64>,
    pub gpu: VecDeque<u64>,
}

pub struct App {
    pub config: Config,
    pub hosts: Vec<HostState>,
    pub topo: Topology,
    pub view: View,
    pub selected: usize,
    pub sort: Sort,
    pub scroll: usize,
    pub frame: u64,
    pub paused: bool,
    /// Click targets from the last frame; later entries sit on top.
    pub hits: RefCell<Vec<(Rect, Hit)>>,
}

impl HostState {
    pub fn new(cfg: Host, local: bool) -> Self {
        Self {
            cfg,
            local,
            status: Status::Connecting,
            snap: None,
            seen: None,
            cpu: VecDeque::new(),
            mem: VecDeque::new(),
            rx: VecDeque::new(),
            tx: VecDeque::new(),
            gpu: VecDeque::new(),
        }
    }

    /// Up, with a reading younger than three intervals.
    pub fn live(&self, interval: Duration) -> bool {
        matches!(self.status, Status::Up) && self.seen.is_some_and(|t| t.elapsed() < interval * 3)
    }
}

impl App {
    pub fn new(config: Config, hosts: Vec<HostState>) -> Self {
        Self {
            config,
            hosts,
            topo: Topology::default(),
            view: View::Overview,
            selected: 0,
            sort: Sort::Cpu,
            scroll: 0,
            frame: 0,
            paused: false,
            hits: RefCell::default(),
        }
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs_f64(self.config.interval)
    }

    pub fn apply(&mut self, update: Update) {
        if self.paused {
            return;
        }
        let cap = self.config.history.max(2);
        match update {
            Update::Snapshot(i, snap) => {
                let h = &mut self.hosts[i];
                let push = |q: &mut VecDeque<u64>, v: u64| {
                    q.push_back(v);
                    while q.len() > cap {
                        q.pop_front();
                    }
                };
                push(&mut h.cpu, snap.cpu.round() as u64);
                push(&mut h.mem, pct(snap.mem_used, snap.mem_total) as u64);
                push(&mut h.rx, snap.rx);
                push(&mut h.tx, snap.tx);
                push(&mut h.gpu, snap.gpus.first().map(|g| g.util.round() as u64).unwrap_or(0));
                h.status = Status::Up;
                h.seen = Some(Instant::now());
                h.snap = Some(*snap);
            }
            Update::Down(i, reason) => self.hosts[i].status = Status::Down(reason),
        }
        let interval = self.interval();
        let snaps: Vec<Option<&Snapshot>> =
            self.hosts.iter().map(|h| h.snap.as_ref().filter(|_| h.live(interval))).collect();
        self.topo = topo::build(&snaps, &self.config);
    }

    /// Returns true to quit.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let n = self.hosts.len().max(1);
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Esc if self.view == View::Host => self.view = View::Overview,
            KeyCode::Esc => return true,
            KeyCode::Char('1') => self.view = View::Overview,
            KeyCode::Char('2') => self.view = View::Map,
            KeyCode::Char('3') => self.view = View::Agents,
            KeyCode::Char('4') | KeyCode::Enter => self.view = View::Host,
            KeyCode::Tab => {
                self.view = match self.view {
                    View::Overview => View::Map,
                    View::Map => View::Agents,
                    View::Agents => View::Host,
                    View::Host => View::Overview,
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.selected = (self.selected + 1) % n;
                self.scroll = 0;
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.selected = (self.selected + n - 1) % n;
                self.scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => self.scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Char('s') => {
                self.sort = if self.sort == Sort::Cpu { Sort::Mem } else { Sort::Cpu };
                self.scroll = 0;
            }
            KeyCode::Char('p') | KeyCode::Char(' ') => self.paused = !self.paused,
            _ => {}
        }
        false
    }
}

impl App {
    pub fn hit(&self, area: Rect, hit: Hit) {
        self.hits.borrow_mut().push((area, hit));
    }

    pub fn mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollDown => self.scroll += 3,
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::Down(MouseButton::Left) => {
                let at = Position { x: m.column, y: m.row };
                let hit = self.hits.borrow().iter().rev().find(|(r, _)| r.contains(at)).map(|(_, h)| *h);
                match hit {
                    Some(Hit::Tab(view)) => {
                        self.view = view;
                        self.scroll = 0;
                    }
                    // A click on the host already selected opens it.
                    Some(Hit::Host(i)) if i == self.selected && self.view != View::Host => self.view = View::Host,
                    Some(Hit::Host(i)) => {
                        self.selected = i;
                        self.scroll = 0;
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }
}

pub fn pct(used: u64, total: u64) -> f64 {
    if total == 0 { 0.0 } else { used as f64 * 100.0 / total as f64 }
}
