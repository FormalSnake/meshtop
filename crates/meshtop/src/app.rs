use std::cell::RefCell;
use std::collections::VecDeque;
use std::f64::consts::PI;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::config::{Account, Config, Host};
use crate::model::Snapshot;
use crate::source::Update;
use crate::topo::{self, Topology};
use crate::usage::Usage;

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
    /// The 3D mesh: drag to rotate, wheel to zoom.
    Graph,
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

/// Where the mesh is seen from. Yaw turns it around the vertical axis, pitch
/// looks down on it from above.
pub struct Camera {
    pub yaw: f64,
    pub pitch: f64,
    pub zoom: f64,
    pub spin: bool,
    /// Yaw and pitch to ease towards, set when a host is selected.
    pub target: Option<(f64, f64)>,
    pub drag: Option<(u16, u16)>,
    /// Last time the user moved the camera; spinning waits a while after it.
    pub touched: Option<Instant>,
}

impl Default for Camera {
    fn default() -> Self {
        Self { yaw: 0.0, pitch: 0.6, zoom: 1.0, spin: true, target: None, drag: None, touched: None }
    }
}

impl Camera {
    const PITCH: f64 = 1.4;

    pub fn tick(&mut self, dt: f64) {
        if let Some((yaw, pitch)) = self.target {
            let k = 1.0 - (-dt * 5.0).exp();
            let dy = (yaw - self.yaw + PI).rem_euclid(2.0 * PI) - PI;
            self.yaw += dy * k;
            self.pitch += (pitch - self.pitch) * k;
            if dy.abs() < 0.002 && (pitch - self.pitch).abs() < 0.002 {
                self.target = None;
            }
        } else if self.spin && self.touched.is_none_or(|t| t.elapsed() > Duration::from_secs(6)) {
            self.yaw += dt * 0.18;
        }
    }

    fn touch(&mut self) {
        self.target = None;
        self.touched = Some(Instant::now());
    }

    pub fn rotate(&mut self, dyaw: f64, dpitch: f64) {
        self.touch();
        self.yaw += dyaw;
        self.pitch = (self.pitch + dpitch).clamp(-0.2, Self::PITCH);
    }

    pub fn zoom_by(&mut self, factor: f64) {
        self.touch();
        self.zoom = (self.zoom * factor).clamp(0.5, 2.5);
    }

    pub fn reset(&mut self) {
        *self = Self { yaw: self.yaw, ..Self::default() };
    }

    /// Turns the mesh until host `i` of `n` is at the front.
    pub fn focus(&mut self, i: usize, n: usize) {
        self.target = Some((crate::ui::floor_angle(i, n), self.pitch));
        self.touched = Some(Instant::now());
    }
}

pub struct AccountState {
    pub account: Account,
    /// `None` while signed out, which hides the account.
    pub reading: Option<Result<Box<Usage>, String>>,
    /// The first window's usage at each reading.
    pub history: VecDeque<u64>,
}

pub struct App {
    pub config: Config,
    pub hosts: Vec<HostState>,
    pub accounts: Vec<AccountState>,
    pub topo: Topology,
    pub view: View,
    pub selected: usize,
    pub sort: Sort,
    pub scroll: usize,
    /// Seconds since start, drives every animation.
    pub clock: f64,
    pub cam: Camera,
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
    pub fn new(config: Config, hosts: Vec<HostState>, accounts: Vec<Account>) -> Self {
        Self {
            config,
            hosts,
            accounts: accounts
                .into_iter()
                .map(|account| AccountState { account, reading: None, history: VecDeque::new() })
                .collect(),
            topo: Topology::default(),
            view: View::Overview,
            selected: 0,
            sort: Sort::Cpu,
            scroll: 0,
            clock: 0.0,
            cam: Camera::default(),
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
            Update::Usage(i, reading) => {
                let a = &mut self.accounts[i];
                match reading.as_ref().and_then(|r| r.as_ref().ok()).and_then(|u| u.windows.first()) {
                    Some(w) => {
                        a.history.push_back(w.used_now().round() as u64);
                        while a.history.len() > cap {
                            a.history.pop_front();
                        }
                    }
                    None if reading.is_none() => a.history.clear(),
                    None => {}
                }
                a.reading = reading;
                return;
            }
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
            KeyCode::Right | KeyCode::Char('l') => self.select((self.selected + 1) % n),
            KeyCode::Left | KeyCode::Char('h') => self.select((self.selected + n - 1) % n),
            KeyCode::Down | KeyCode::Char('j') if self.view == View::Map => self.cam.rotate(0.0, -0.1),
            KeyCode::Up | KeyCode::Char('k') if self.view == View::Map => self.cam.rotate(0.0, 0.1),
            KeyCode::Char('+') | KeyCode::Char('=') => self.cam.zoom_by(1.15),
            KeyCode::Char('-') => self.cam.zoom_by(1.0 / 1.15),
            KeyCode::Char('r') => {
                let spin = !self.cam.spin;
                self.cam.reset();
                self.cam.spin = spin;
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
    pub fn select(&mut self, i: usize) {
        self.selected = i;
        self.scroll = 0;
        self.cam.focus(i, self.hosts.len());
    }

    pub fn tick(&mut self, dt: f64) {
        self.clock += dt;
        self.cam.tick(dt);
    }

    pub fn hit(&self, area: Rect, hit: Hit) {
        self.hits.borrow_mut().push((area, hit));
    }

    pub fn mouse(&mut self, m: MouseEvent) {
        let at = Position { x: m.column, y: m.row };
        let hit = self.hits.borrow().iter().rev().find(|(r, _)| r.contains(at)).map(|(_, h)| *h);
        let on_graph = self.hits.borrow().iter().any(|(r, h)| matches!(h, Hit::Graph) && r.contains(at));
        match m.kind {
            MouseEventKind::ScrollDown if on_graph => self.cam.zoom_by(1.0 / 1.1),
            MouseEventKind::ScrollUp if on_graph => self.cam.zoom_by(1.1),
            MouseEventKind::ScrollDown => self.scroll += 3,
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((x, y)) = self.cam.drag {
                    let (dx, dy) = (m.column as f64 - x as f64, m.row as f64 - y as f64);
                    // Cells are about twice as tall as wide.
                    self.cam.rotate(-dx * 0.05, dy * 0.1);
                    self.cam.drag = Some((m.column, m.row));
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.cam.drag = None,
            MouseEventKind::Down(MouseButton::Left) => {
                if on_graph {
                    self.cam.drag = Some((m.column, m.row));
                }
                match hit {
                    Some(Hit::Tab(view)) => {
                        self.view = view;
                        self.scroll = 0;
                    }
                    // A click on the host already selected opens it.
                    Some(Hit::Host(i)) if i == self.selected && self.view != View::Host => self.view = View::Host,
                    Some(Hit::Host(i)) => self.select(i),
                    Some(Hit::Graph) | None => {}
                }
            }
            _ => {}
        }
    }
}

pub fn pct(used: u64, total: u64) -> f64 {
    if total == 0 { 0.0 } else { used as f64 * 100.0 / total as f64 }
}
