use crate::model::Battery;

#[cfg(target_os = "linux")]
pub fn read() -> Option<Battery> {
    let mut battery = None;
    let mut ac = false;
    for entry in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let path = entry.path();
        let read = |f: &str| std::fs::read_to_string(path.join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
        match read("type").as_str() {
            "Mains" => ac |= read("online") == "1",
            "Battery" if read("scope") != "Device" => {
                let percent = read("capacity").parse().ok()?;
                battery = Some(Battery { percent, charging: read("status") == "Charging", ac: false });
            }
            _ => {}
        }
    }
    battery.map(|b| Battery { ac, ..b })
}

/// `pmset -g batt`: "Now drawing from 'AC Power'" then
/// "-InternalBattery-0 (id=…)<TAB>90%; charging; 1:02 remaining present: true".
#[cfg(target_os = "macos")]
pub fn read() -> Option<Battery> {
    let out = std::process::Command::new("/usr/bin/pmset").args(["-g", "batt"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.contains("InternalBattery"))?;
    let percent = line.split('\t').nth(1)?.split('%').next()?.trim().parse().ok()?;
    Some(Battery { percent, charging: line.contains("; charging;"), ac: text.contains("'AC Power'") })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn read() -> Option<Battery> {
    None
}
