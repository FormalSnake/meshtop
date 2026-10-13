mod app;
mod collect;
mod config;
mod model;
mod source;
mod topo;
mod ui;
mod usage;

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use ratatui::crossterm::execute;

use app::{App, HostState};
use collect::Collector;

#[derive(Parser)]
#[command(version, about = "Live resource usage and connections across your machines")]
struct Cli {
    /// Config file, default $XDG_CONFIG_HOME/meshtop/config.toml.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print a JSON snapshot of this machine on every interval. The TUI runs
    /// this over ssh on each remote host.
    Agent {
        /// Seconds between snapshots.
        #[arg(short, long, default_value_t = 1.0)]
        interval: f64,
    },
    /// Print one snapshot of this machine as pretty JSON.
    Snapshot,
    /// Print the config path and the hosts it resolves to.
    Hosts,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::load(cli.config)?;
    match cli.command {
        Some(Cmd::Agent { interval }) => agent(config, interval),
        Some(Cmd::Snapshot) => {
            let mut c = Collector::new(config.activity);
            std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(500)));
            println!("{}", serde_json::to_string_pretty(&c.sample())?);
            Ok(())
        }
        Some(Cmd::Hosts) => {
            let local = sysinfo::System::host_name().unwrap_or_default();
            println!("{}", config::path().display());
            for h in resolve_hosts(&config, &local) {
                let how = if h.local {
                    "local".to_string()
                } else {
                    format!("ssh {} {}", h.cfg.destination(), h.cfg.command)
                };
                println!("{:<16} {how}", h.cfg.name);
            }
            Ok(())
        }
        None => tui(config),
    }
}

fn agent(config: config::Config, interval: f64) -> Result<()> {
    let interval = Duration::from_secs_f64(interval.max(0.2));
    let mut collector = Collector::new(config.activity);
    let mut out = std::io::stdout().lock();
    let mut wait = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL;
    loop {
        std::thread::sleep(wait);
        wait = interval;
        let line = serde_json::to_string(&collector.sample())?;
        // A closed pipe means the TUI went away; exit quietly.
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            return Ok(());
        }
    }
}

/// The configured hosts, or just this machine when none are configured.
fn resolve_hosts(config: &config::Config, local: &str) -> Vec<HostState> {
    if config.hosts.is_empty() {
        let name = local.split('.').next().unwrap_or(local).to_string();
        let host = config::Host { name, ssh: None, command: "meshtop".into(), role: None, hostname: None };
        return vec![HostState::new(host, true)];
    }
    config.hosts.iter().map(|h| HostState::new(h.clone(), h.is_local(local))).collect()
}

fn tui(config: config::Config) -> Result<()> {
    let local = sysinfo::System::host_name().unwrap_or_default();
    let hosts = resolve_hosts(&config, &local);
    let interval = Duration::from_secs_f64(config.interval.max(0.2));

    let (tx, rx) = mpsc::channel();
    let children = source::Children::default();
    for (i, h) in hosts.iter().enumerate() {
        if h.local {
            source::spawn_local(i, config.activity.clone(), interval, tx.clone());
        } else {
            source::spawn_remote(i, h.cfg.clone(), interval, tx.clone(), children.clone());
        }
    }
    let accounts = usage::accounts(&config.usage);
    usage::spawn(accounts.clone(), tx.clone());
    drop(tx);

    let mut app = App::new(config, hosts, accounts);
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture)?;
    let result = (|| -> Result<()> {
        let mut last = Instant::now();
        loop {
            while let Ok(update) = rx.try_recv() {
                app.apply(update);
            }
            let now = Instant::now();
            app.tick((now - last).as_secs_f64());
            last = now;
            terminal.draw(|f| ui::draw(f, &app))?;
            if event::poll(Duration::from_millis(33))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press && app.key(key) => return Ok(()),
                    Event::Mouse(m) => app.mouse(m),
                    _ => {}
                }
            }
        }
    })();
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    children.kill_all();
    result
}
