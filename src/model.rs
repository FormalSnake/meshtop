use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// One reading of a machine, written by `meshtop agent` as a JSON line.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub arch: String,
    pub cpu_brand: String,
    pub uptime: u64,
    /// Unix time in milliseconds.
    pub time: u64,

    pub cpu: f32,
    pub cores: Vec<f32>,
    pub load: [f64; 3],
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,

    /// Bytes per second, summed over physical interfaces.
    pub rx: u64,
    pub tx: u64,
    pub ifaces: Vec<Iface>,
    pub addrs: Vec<IpAddr>,

    pub disks: Vec<Disk>,
    pub temps: Vec<Temp>,
    pub gpus: Vec<Gpu>,
    pub battery: Option<Battery>,

    pub process_count: usize,
    pub procs: Vec<Proc>,
    pub activity: Vec<Activity>,
    pub agents: Vec<Agent>,
    pub conns: Vec<Conn>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Iface {
    pub name: String,
    pub rx: u64,
    pub tx: u64,
    pub addrs: Vec<IpAddr>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    pub mount: String,
    pub fs: String,
    pub used: u64,
    pub total: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Temp {
    pub label: String,
    pub celsius: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gpu {
    pub name: String,
    pub util: f32,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub celsius: Option<f32>,
    pub watts: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Battery {
    pub percent: f32,
    pub charging: bool,
    pub ac: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proc {
    pub pid: u32,
    pub name: String,
    pub user: String,
    pub cpu: f32,
    pub mem: u64,
    pub cmd: String,
}

/// A count of processes matching one activity rule, e.g. `claude ×4`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Activity {
    pub label: String,
    pub count: usize,
}

/// An established TCP connection that does not stay on loopback.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conn {
    pub local: IpAddr,
    pub local_port: u16,
    pub remote: IpAddr,
    pub remote_port: u16,
    pub process: Option<String>,
    /// Socket owner, only known on Linux.
    pub uid: Option<u32>,
}

/// A coding agent session, from Herdr when it runs there.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    /// claude, codex, …
    pub kind: String,
    /// idle, working, blocked, done or unknown.
    pub status: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub cwd: String,
    pub cpu: f32,
    pub mem: u64,
    pub herdr: bool,
}
