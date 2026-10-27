use std::path::{Path, PathBuf};
use std::process::Command;

use super::find;
use crate::model::Gpu;

const MIB: u64 = 1024 * 1024;

pub enum Gpus {
    Nvidia(PathBuf),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Amd(Vec<PathBuf>),
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Apple,
    None,
}

impl Gpus {
    pub fn detect() -> Self {
        if cfg!(target_os = "macos") {
            return Self::Apple;
        }
        if let Some(smi) = find("nvidia-smi") {
            return Self::Nvidia(smi);
        }
        let amd: Vec<PathBuf> = std::fs::read_dir("/sys/class/drm")
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join("device"))
            .filter(|d| d.join("gpu_busy_percent").exists())
            .collect();
        if amd.is_empty() { Self::None } else { Self::Amd(amd) }
    }

    pub fn read(&self) -> Vec<Gpu> {
        match self {
            Self::Nvidia(smi) => nvidia(smi),
            Self::Amd(devices) => devices.iter().filter_map(|d| amd(d)).collect(),
            Self::Apple => apple().into_iter().collect(),
            Self::None => Vec::new(),
        }
    }
}

fn nvidia(smi: &Path) -> Vec<Gpu> {
    let Ok(out) = Command::new(smi)
        .args([
            "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
            "--format=csv,noheader,nounits",
        ])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok());
            Some(Gpu {
                name: f.first()?.trim_start_matches("NVIDIA ").trim_start_matches("GeForce ").to_string(),
                util: num(1)? as f32,
                mem_used: num(2).map(|v| v as u64 * MIB),
                mem_total: num(3).map(|v| v as u64 * MIB),
                celsius: num(4).map(|v| v as f32),
                watts: num(5).map(|v| v as f32),
            })
        })
        .collect()
}

fn amd(device: &Path) -> Option<Gpu> {
    let read = |f: &str| std::fs::read_to_string(device.join(f)).ok()?.trim().parse::<u64>().ok();
    let hwmon = std::fs::read_dir(device.join("hwmon")).ok()?.flatten().next().map(|e| e.path());
    let hw = |f: &str| hwmon.as_ref().and_then(|h| std::fs::read_to_string(h.join(f)).ok()?.trim().parse::<u64>().ok());
    Some(Gpu {
        name: "AMD GPU".into(),
        util: read("gpu_busy_percent")? as f32,
        mem_used: read("mem_info_vram_used"),
        mem_total: read("mem_info_vram_total"),
        celsius: hw("temp1_input").map(|m| m as f32 / 1000.0),
        watts: hw("power1_average").map(|u| u as f32 / 1_000_000.0),
    })
}

/// The AGX driver publishes utilisation in its IORegistry performance
/// statistics, readable without root.
fn apple() -> Option<Gpu> {
    let out = Command::new("/usr/sbin/ioreg").args(["-r", "-d", "1", "-c", "IOAccelerator"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let int = |key: &str| {
        let at = text.find(&format!("\"{key}\"="))? + key.len() + 3;
        text[at..].split(|c: char| !c.is_ascii_digit()).next()?.parse::<u64>().ok()
    };
    let model =
        text.find("\"model\" = \"").and_then(|at| text[at + 11..].split('"').next()).unwrap_or("Apple GPU").to_string();
    Some(Gpu {
        name: model,
        util: int("Device Utilization %")? as f32,
        mem_used: int("In use system memory"),
        mem_total: None,
        celsius: None,
        watts: None,
    })
}
