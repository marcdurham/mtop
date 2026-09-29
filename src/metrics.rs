use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System};

/// Pseudo / virtual filesystems that don't represent real storage.
const IGNORED_FS: &[&str] = &[
    "tmpfs", "devtmpfs", "squashfs", "overlay", "proc", "sysfs", "cgroup", "cgroup2", "devpts",
    "debugfs", "tracefs", "securityfs", "pstore", "bpf", "autofs", "mqueue", "hugetlbfs",
    "fusectl", "configfs", "binfmt_misc", "ramfs", "nsfs", "efivarfs", "fuse.snapfuse",
    "fuse.portal", "fuse.gvfsd-fuse",
];

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub timestamp: u64,
    pub hostname: Option<String>,
    pub uptime_secs: u64,
    pub cpu: CpuStats,
    pub memory: MemoryStats,
    pub disks: Vec<DiskStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CpuStats {
    pub usage_percent: f32,
    pub core_count: usize,
    pub per_core_percent: Vec<f32>,
    pub load_average: LoadAverage,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadAverage {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryStats {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub usage_percent: f64,
    pub swap: SwapStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct SwapStats {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub usage_percent: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskStats {
    pub mount_point: String,
    pub device: String,
    pub file_system: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub usage_percent: f64,
    pub removable: bool,
}

/// Compact per-sample record kept in the history ring buffer.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryPoint {
    pub timestamp: u64,
    pub cpu_percent: f32,
    pub load_one: f64,
    pub memory_percent: f64,
    pub swap_percent: f64,
    pub disks: Vec<DiskUsagePoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskUsagePoint {
    pub mount_point: String,
    pub usage_percent: f64,
}

impl From<&Snapshot> for HistoryPoint {
    fn from(s: &Snapshot) -> Self {
        Self {
            timestamp: s.timestamp,
            cpu_percent: s.cpu.usage_percent,
            load_one: s.cpu.load_average.one,
            memory_percent: s.memory.usage_percent,
            swap_percent: s.memory.swap.usage_percent,
            disks: s
                .disks
                .iter()
                .map(|d| DiskUsagePoint {
                    mount_point: d.mount_point.clone(),
                    usage_percent: d.usage_percent,
                })
                .collect(),
        }
    }
}

/// Fixed-capacity ring buffer of history points.
pub struct History {
    points: VecDeque<HistoryPoint>,
    capacity: usize,
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self {
            points: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, point: HistoryPoint) {
        if self.points.len() == self.capacity {
            self.points.pop_front();
        }
        self.points.push_back(point);
    }

    /// Points with a timestamp at or after `since` (all points if `None`).
    pub fn since(&self, since: Option<u64>) -> Vec<HistoryPoint> {
        let since = since.unwrap_or(0);
        self.points
            .iter()
            .filter(|p| p.timestamp >= since)
            .cloned()
            .collect()
    }
}

/// Owns the sysinfo handles. CPU usage is computed as a delta between
/// refreshes, so the collector must be long-lived and sampled periodically.
pub struct Collector {
    sys: System,
}

impl Collector {
    pub fn new() -> Self {
        let mut sys = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
                .with_memory(MemoryRefreshKind::everything()),
        );
        // Prime the CPU counters so the first real sample has a baseline.
        sys.refresh_cpu_usage();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        Self { sys }
    }

    pub fn sample(&mut self) -> Snapshot {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();

        let load = System::load_average();
        let cpu = CpuStats {
            usage_percent: round1(self.sys.global_cpu_usage() as f64) as f32,
            core_count: self.sys.cpus().len(),
            per_core_percent: self
                .sys
                .cpus()
                .iter()
                .map(|c| round1(c.cpu_usage() as f64) as f32)
                .collect(),
            load_average: LoadAverage {
                one: load.one,
                five: load.five,
                fifteen: load.fifteen,
            },
        };

        let total = self.sys.total_memory();
        let available = self.sys.available_memory();
        let used = total.saturating_sub(available);
        let swap_total = self.sys.total_swap();
        let swap_used = self.sys.used_swap();
        let memory = MemoryStats {
            total_bytes: total,
            used_bytes: used,
            available_bytes: available,
            usage_percent: percent(used, total),
            swap: SwapStats {
                total_bytes: swap_total,
                used_bytes: swap_used,
                free_bytes: swap_total.saturating_sub(swap_used),
                usage_percent: percent(swap_used, swap_total),
            },
        };

        Snapshot {
            timestamp: unix_now(),
            hostname: System::host_name(),
            uptime_secs: System::uptime(),
            cpu,
            memory,
            disks: collect_disks(),
        }
    }
}

/// Real, mounted filesystems. The mount list is re-read every sample so
/// hot-plugged drives appear and disappear.
fn collect_disks() -> Vec<DiskStats> {
    let disks = Disks::new_with_refreshed_list();
    // The same device can be mounted multiple times (bind mounts, systemd
    // sandboxing, btrfs subvolumes). Keep the shortest mount point per device.
    let mut by_device: HashMap<String, DiskStats> = HashMap::new();

    for disk in disks.list() {
        let fs = disk.file_system().to_string_lossy().into_owned();
        let total = disk.total_space();
        if total == 0 || IGNORED_FS.contains(&fs.as_str()) {
            continue;
        }
        let available = disk.available_space();
        let used = total.saturating_sub(available);
        let stats = DiskStats {
            mount_point: disk.mount_point().to_string_lossy().into_owned(),
            device: disk.name().to_string_lossy().into_owned(),
            file_system: fs,
            total_bytes: total,
            used_bytes: used,
            available_bytes: available,
            usage_percent: percent(used, total),
            removable: disk.is_removable(),
        };
        match by_device.get(&stats.device) {
            Some(existing) if existing.mount_point.len() <= stats.mount_point.len() => {}
            _ => {
                by_device.insert(stats.device.clone(), stats);
            }
        }
    }

    let mut out: Vec<DiskStats> = by_device.into_values().collect();
    out.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    out
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        round1(part as f64 / whole as f64 * 100.0)
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
