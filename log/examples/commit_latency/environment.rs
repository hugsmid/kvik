//! The environment the commit latency run reports: platform, file system, disk class, sync primitive and load,
//! without names, serials or paths.

use std::path::Path;
use std::process::Command;

fn run_cmd(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn read_trim(p: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

pub(crate) fn os_version() -> String {
    let v = match std::env::consts::OS {
        "linux" => read_trim("/proc/sys/kernel/osrelease").map(|k| format!("kernel {k}")),
        "macos" => run_cmd("sw_vers", &["-productVersion"]).map(|s| s.trim().to_owned()),
        "windows" => run_cmd("cmd", &["/C", "ver"]).map(|s| s.trim().to_owned()),
        _ => None,
    };
    v.unwrap_or_else(|| "unknown".into())
}

pub(crate) fn sync_primitive() -> &'static str {
    match std::env::consts::OS {
        "linux" => {
            "batch and mark: `fdatasync` (`File::sync_data`); creation and seal: `fsync`; directory: `fsync`"
        }
        "macos" => {
            "batch and mark: `fcntl(F_FULLFSYNC)` (`File::sync_data`); creation, seal and directory: the same"
        }
        "windows" => {
            "batch and mark: `FlushFileBuffers` (`File::sync_data`); no directory sync: a created or renamed file is flushed"
        }
        _ => "`File::sync_data` and `File::sync_all`",
    }
}

/// The file system and the class of disk under `dir`, without names, serials or paths.
pub(crate) fn storage(dir: &Path) -> (String, String) {
    let unknown = || "unknown".to_owned();
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
    match std::env::consts::OS {
        "linux" => linux_storage(&dir).unwrap_or_else(|| (unknown(), unknown())),
        "macos" => macos_storage(&dir).unwrap_or_else(|| (unknown(), unknown())),
        "windows" => windows_storage(&dir).unwrap_or_else(|| (unknown(), unknown())),
        _ => (unknown(), unknown()),
    }
}

fn linux_storage(dir: &Path) -> Option<(String, String)> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let d = dir.to_string_lossy();
    let mut best: Option<(usize, String, String)> = None;
    for line in info.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        let Some(sep) = f.iter().position(|x| *x == "-") else {
            continue;
        };
        if f.len() < 5 || f.len() <= sep + 1 {
            continue;
        }
        let mp = f[4].replace("\\040", " ");
        let under = mp == "/" || d == mp.as_str() || d.starts_with(&format!("{mp}/"));
        if under && best.as_ref().is_none_or(|(l, _, _)| mp.len() >= *l) {
            best = Some((mp.len(), f[sep + 1].to_owned(), f[2].to_owned()));
        }
    }
    let (_, fstype, majmin) = best?;
    let dev = Path::new("/sys/dev/block")
        .join(&majmin)
        .canonicalize()
        .ok();
    let disk = dev.map(|d| {
        if d.join("partition").exists() {
            d.parent().map(Path::to_path_buf).unwrap_or(d)
        } else {
            d
        }
    });
    let disk = match disk {
        Some(d) => {
            let name = d
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let class = [
                ("nvme", "NVMe"),
                ("sd", "SCSI or SATA"),
                ("vd", "virtio"),
                ("xvd", "Xen virtual"),
                ("dm-", "device mapper"),
                ("md", "software RAID"),
                ("loop", "loop device"),
                ("mmcblk", "MMC/SD card"),
            ]
            .iter()
            .find(|(p, _)| name.starts_with(p))
            .map_or("other", |(_, c)| c);
            let q = d.join("queue");
            let rot = match read_trim(q.join("rotational")).as_deref() {
                Some("0") => "solid state",
                Some("1") => "rotational",
                _ => "rotational unknown",
            };
            let cache = read_trim(q.join("write_cache")).unwrap_or_else(|| "unknown".into());
            let fua = match read_trim(q.join("fua")).as_deref() {
                Some("1") => ", FUA",
                Some("0") => ", no FUA",
                _ => "",
            };
            format!("{class}, {rot}, write cache {cache}{fua}")
        }
        None => "unknown".into(),
    };
    Some((fstype, disk))
}

fn macos_storage(dir: &Path) -> Option<(String, String)> {
    let df = run_cmd("df", &["-P", &dir.to_string_lossy()])?;
    let device = df.lines().nth(1)?.split_whitespace().next()?.to_owned();
    let mounts = run_cmd("mount", &[]).unwrap_or_default();
    let fstype = mounts
        .lines()
        .find(|l| l.starts_with(&format!("{device} on ")))
        .and_then(|l| l.split_once(" ("))
        .and_then(|(_, rest)| rest.split([',', ')']).next())
        .map_or_else(|| "unknown".into(), |s| s.trim().to_owned());
    let disk = run_cmd("diskutil", &["info", &device]).map(|info| {
        let field = |key: &str| {
            info.lines()
                .find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_owned()))
        };
        let proto = field("Protocol:").unwrap_or_default();
        let class = if proto.contains("USB") {
            "USB"
        } else if proto.contains("SATA") {
            "SATA"
        } else if proto.contains("PCI") || proto.contains("NVMe") || proto.contains("Fabric") {
            "NVMe"
        } else {
            "other"
        };
        let ssd = match field("Solid State:").as_deref() {
            Some("Yes") => "solid state",
            Some("No") => "rotational",
            _ => "solid state unknown",
        };
        let loc = field("Device Location:").map_or("location unknown".into(), |l| l.to_lowercase());
        format!("{class}, {ssd}, {loc}")
    });
    Some((fstype, disk.unwrap_or_else(|| "unknown".into())))
}

fn windows_storage(dir: &Path) -> Option<(String, String)> {
    let s = dir.to_string_lossy();
    let letter = s
        .char_indices()
        .find(|&(i, c)| c.is_ascii_alphabetic() && s[i + 1..].starts_with(':'))?
        .1;
    let script = format!(
        "$v = Get-Volume -DriveLetter {letter}; $d = Get-Partition -DriveLetter {letter} | Get-Disk; \
         $p = Get-PhysicalDisk | Where-Object DeviceId -eq $d.Number; \
         Write-Output $v.FileSystemType; Write-Output $d.BusType; Write-Output $p.MediaType"
    );
    let out = run_cmd("powershell", &["-NoProfile", "-Command", &script])?;
    let mut lines = out.lines().map(str::trim);
    let fstype = lines.next().filter(|s| !s.is_empty())?.to_owned();
    let bus = lines.next().unwrap_or("unknown").to_owned();
    let media = lines.next().unwrap_or("unknown").to_owned();
    Some((fstype, format!("bus {bus}, media {media}")))
}

pub(crate) fn load_average() -> String {
    let v = match std::env::consts::OS {
        "linux" => read_trim("/proc/loadavg")
            .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" ")),
        "macos" => run_cmd("sysctl", &["-n", "vm.loadavg"])
            .map(|s| s.trim().trim_matches(['{', '}']).trim().to_owned())
            .or_else(|| {
                let up = run_cmd("uptime", &[])?;
                let (_, avg) = up.split_once("load average")?;
                Some(
                    avg.trim_start_matches(['s', ':', ' '])
                        .trim()
                        .replace(',', ""),
                )
            }),
        _ => None,
    };
    v.unwrap_or_else(|| "not available".into())
}
