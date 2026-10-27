mod agents;
mod battery;
mod conns;
mod gpu;

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sysinfo::{
    Components, CpuRefreshKind, Disks, MemoryRefreshKind, Networks, ProcessRefreshKind, ProcessesToUpdate, RefreshKind,
    System, UpdateKind, Users,
};

use crate::config::Rule;
use crate::model::{Activity, Disk, Iface, Proc, Snapshot, Temp};

const TOP_PROCS: usize = 40;
const SLOW_EVERY: Duration = Duration::from_secs(10);

pub struct Collector {
    sys: System,
    networks: Networks,
    disks: Disks,
    components: Components,
    users: Users,
    rules: Vec<Rule>,
    gpu: gpu::Gpus,
    battery: Option<crate::model::Battery>,
    herdr: Option<PathBuf>,
    last: Instant,
    last_slow: Option<Instant>,
}

impl Collector {
    pub fn new(rules: Vec<Rule>) -> Self {
        let sys = System::new_with_specifics(
            RefreshKind::nothing().with_cpu(CpuRefreshKind::everything()).with_memory(MemoryRefreshKind::everything()),
        );
        let mut c = Self {
            sys,
            networks: Networks::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
            components: Components::new_with_refreshed_list(),
            users: Users::new_with_refreshed_list(),
            rules,
            gpu: gpu::Gpus::detect(),
            battery: None,
            herdr: std::env::var_os("HERDR_BIN_PATH").map(PathBuf::from).or_else(|| find("herdr")),
            last: Instant::now(),
            last_slow: None,
        };
        c.refresh_procs();
        c
    }

    fn refresh_procs(&mut self) {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_user(UpdateKind::OnlyIfNotSet)
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_cwd(UpdateKind::OnlyIfNotSet),
        );
    }

    /// Takes one reading. CPU and network figures cover the time since the
    /// previous call, so the first one after `new` should come at least
    /// `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL` later.
    pub fn sample(&mut self) -> Snapshot {
        let elapsed = self.last.elapsed().as_secs_f64().max(0.001);
        self.last = Instant::now();

        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();
        self.refresh_procs();
        self.networks.refresh(true);
        self.components.refresh(true);

        if self.last_slow.is_none_or(|t| t.elapsed() >= SLOW_EVERY) {
            self.last_slow = Some(Instant::now());
            self.disks.refresh(true);
            self.users.refresh();
            self.battery = battery::read();
        }

        let load = System::load_average();
        let mut snap = Snapshot {
            hostname: System::host_name().unwrap_or_default(),
            os: System::long_os_version().unwrap_or_default(),
            kernel: System::kernel_version().unwrap_or_default(),
            arch: System::cpu_arch(),
            cpu_brand: self.sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default(),
            uptime: System::uptime(),
            time: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
            cpu: self.sys.global_cpu_usage(),
            cores: self.sys.cpus().iter().map(|c| c.cpu_usage()).collect(),
            load: [load.one, load.five, load.fifteen],
            mem_used: self.sys.used_memory(),
            mem_total: self.sys.total_memory(),
            swap_used: self.sys.used_swap(),
            swap_total: self.sys.total_swap(),
            gpus: self.gpu.read(),
            battery: self.battery.clone(),
            process_count: self.sys.processes().len(),
            ..Default::default()
        };

        self.networking(&mut snap, elapsed);
        self.storage(&mut snap);
        self.temperatures(&mut snap);
        self.processes(&mut snap);
        snap.agents = agents::read(self.herdr.as_deref(), &self.sys);
        snap.conns = conns::read(&self.sys);
        // A connection to one of our own addresses never leaves the machine.
        snap.conns.retain(|c| !snap.addrs.contains(&c.remote));
        snap
    }

    fn networking(&self, snap: &mut Snapshot, elapsed: f64) {
        let rate = |bytes: u64| (bytes as f64 / elapsed) as u64;
        for (name, data) in self.networks.list() {
            if name.starts_with("lo") {
                continue;
            }
            let addrs: Vec<IpAddr> =
                data.ip_networks().iter().map(|n| n.addr).filter(|a| !a.is_loopback() && !link_local(a)).collect();
            snap.addrs.extend(&addrs);
            let (rx, tx) = (rate(data.received()), rate(data.transmitted()));
            if !virtual_iface(name) {
                snap.rx += rx;
                snap.tx += tx;
            }
            if !addrs.is_empty() || rx + tx > 0 {
                snap.ifaces.push(Iface { name: name.clone(), rx, tx, addrs });
            }
        }
        snap.ifaces.sort_by(|a, b| (b.rx + b.tx).cmp(&(a.rx + a.tx)).then(a.name.cmp(&b.name)));
        snap.addrs.sort();
        snap.addrs.dedup();
    }

    fn storage(&self, snap: &mut Snapshot) {
        let mut seen = HashSet::new();
        for d in self.disks.list() {
            let mount = d.mount_point().to_string_lossy().to_string();
            let fs = d.file_system().to_string_lossy().to_string();
            let hidden = matches!(fs.as_str(), "tmpfs" | "devtmpfs" | "overlay" | "squashfs" | "ramfs" | "autofs")
                || mount.starts_with("/System/Volumes/")
                || mount.starts_with("/private/var/")
                || mount.starts_with("/run/")
                || d.total_space() == 0;
            // Btrfs subvolumes and APFS volumes share one pool; show it once.
            if hidden || !seen.insert((d.name().to_owned(), d.total_space())) {
                continue;
            }
            snap.disks.push(Disk { mount, fs, used: d.total_space() - d.available_space(), total: d.total_space() });
        }
        snap.disks.sort_by(|a, b| (a.mount != "/").cmp(&(b.mount != "/")).then(a.mount.cmp(&b.mount)));
    }

    fn temperatures(&self, snap: &mut Snapshot) {
        snap.temps = self
            .components
            .list()
            .iter()
            .filter_map(|c| Some(Temp { label: c.label().to_string(), celsius: c.temperature()? }))
            .filter(|t| t.celsius.is_finite() && t.celsius > 0.0 && t.celsius < 150.0)
            .collect();
        snap.temps.sort_by(|a, b| b.celsius.total_cmp(&a.celsius));
        snap.temps.truncate(12);
    }

    fn processes(&self, snap: &mut Snapshot) {
        let user_names: HashMap<_, _> = self.users.list().iter().map(|u| (u.id().clone(), u.name())).collect();
        let mut counts: Vec<(usize, HashSet<String>)> = vec![(0, HashSet::new()); self.rules.len()];
        let mut procs = Vec::with_capacity(self.sys.processes().len());

        for (pid, p) in self.sys.processes() {
            // Linux lists every thread as a task of its process; skip those.
            if p.thread_kind().is_some_and(|k| k == sysinfo::ThreadKind::Userland) {
                continue;
            }
            let name = p.name().to_string_lossy().to_string();
            let user = p.user_id().and_then(|u| user_names.get(u)).map(|s| s.to_string()).unwrap_or_default();
            let cmd = p.cmd().iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>().join(" ");

            for (rule, count) in self.rules.iter().zip(counts.iter_mut()) {
                let ok = (rule.name.is_empty() || rule.name.contains(&name))
                    && rule.cmd.as_ref().is_none_or(|c| cmd.contains(c.as_str()))
                    && rule.user.as_ref().is_none_or(|u| user.starts_with(u.as_str()));
                if ok {
                    count.0 += 1;
                    if rule.user.is_some() {
                        count.1.insert(user.clone());
                    }
                }
            }
            procs.push(Proc { pid: pid.as_u32(), name, user, cpu: p.cpu_usage(), mem: p.memory(), cmd });
        }

        let mut by_label: Vec<Activity> = Vec::new();
        for (rule, (n, users)) in self.rules.iter().zip(counts) {
            let count = if rule.user.is_some() { users.len() } else { n };
            match by_label.iter_mut().find(|a| a.label == rule.label) {
                Some(a) => a.count += count,
                None => by_label.push(Activity { label: rule.label.clone(), count }),
            }
        }
        snap.activity = by_label.into_iter().filter(|a| a.count > 0).collect();

        procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        let mut top: Vec<Proc> = procs.drain(..TOP_PROCS.min(procs.len())).collect();
        procs.sort_by_key(|p| std::cmp::Reverse(p.mem));
        top.extend(procs.into_iter().take(TOP_PROCS / 2));
        snap.procs = top;
    }
}

pub(crate) fn find(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain([PathBuf::from("/run/current-system/sw/bin")])
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

fn link_local(a: &IpAddr) -> bool {
    match a {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Tunnels, bridges and VM links carry traffic a physical link already
/// counted, so they stay out of the host total.
fn virtual_iface(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "tailscale",
        "utun",
        "tun",
        "tap",
        "wg",
        "veth",
        "docker",
        "br-",
        "virbr",
        "vnet",
        "vmnet",
        "vmenet",
        "bridge",
        "awdl",
        "llw",
        "anpi",
        "ap",
        "gif",
        "stf",
        "zt",
        "podman",
        "cni",
        "flannel",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}
