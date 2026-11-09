use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sysinfo::{Disks, MINIMUM_CPU_UPDATE_INTERVAL, System};

use super::SandboxCeiling;

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

// Short on purpose: a sample every ten seconds, read at p75, means a spike of a few
// samples is not a machine out of room, while a machine that recovered is asked again
// in a minute rather than after the operator has given up on it.
pub const WINDOW: Duration = Duration::from_secs(90);

// Where the valve shuts and where it opens again. Two thresholds because one flaps: a
// machine sitting on the line would take a task, become too busy, refuse the next, and
// spend the afternoon taking every other one.
pub const BUSY_CPU_PERCENT: f64 = 75.0;
pub const CALM_CPU_PERCENT: f64 = 60.0;
pub const BUSY_MEMORY_PERCENT: f64 = 80.0;
pub const CALM_MEMORY_PERCENT: f64 = 70.0;

// A disk that fills stops a machine rather than slowing it, and it is the one resource
// a ceiling can take past the point of no return. Five gigabytes is a machine in
// trouble, whatever its disk's size.
pub const DISK_FLOOR_MIB: u64 = 5 * 1_024;

pub const MIN_READINGS: usize = 4;

// What one sandbox's disk may grow to. A ceiling is a bound rather than a reservation,
// and the disk is the one resource whose exhaustion is a machine stopping mid-build
// instead of slowing down, so it is sized for a Rust workspace with room to spare.
pub const SANDBOX_DISK_CEILING_MIB: u32 = 30 * 1_024;

pub const MEMORY_SHARE_OF_HOST: u64 = 80;
pub const MINIMUM_CEILING_MIB: u64 = 2 * 1_024;
pub const MAXIMUM_CEILING_MIB: u64 = 8 * 1_024;
pub const MAXIMUM_CEILING_CPUS: u8 = 4;

// What a machine is too busy with. A ceiling is a bound and not a reservation, so
// there is no count of what fits anywhere in this module: the only question a queue
// can ask a host is whether it is coping, and this is the answer when it is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressure {
    Cpu,
    Memory,
    Disk,
}

// How many starts may be landing at once. A start is minutes of boot before it shows
// up in the machine's own load, so this counts transitions and not capacity: two at a
// time on a machine with the cores to absorb them, one otherwise.
pub fn landings() -> usize {
    thread::available_parallelism()
        .map_or(1, |cores| cores.get())
        .min(2)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceSample {
    pub at_ms: i64,
    pub cpu_percent: f64,
    pub memory_percent: f64,
    pub available_mib: u64,
}

#[derive(Debug, Default)]
pub struct SampleWindow {
    samples: VecDeque<ResourceSample>,
}

impl SampleWindow {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, sample: ResourceSample) {
        let window_ms = i64::try_from(WINDOW.as_millis()).unwrap_or(i64::MAX);
        let cutoff = sample.at_ms.saturating_sub(window_ms);
        while self.samples.front().is_some_and(|old| old.at_ms < cutoff) {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn is_warming_up(&self) -> bool {
        self.samples.len() < MIN_READINGS
    }

    pub fn latest(&self) -> Option<&ResourceSample> {
        self.samples.back()
    }

    pub fn cpu_p75(&self) -> Option<f64> {
        percentile_75(self.samples.iter().map(|sample| sample.cpu_percent))
    }

    pub fn memory_p75(&self) -> Option<f64> {
        percentile_75(self.samples.iter().map(|sample| sample.memory_percent))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostTotals {
    pub cpus: usize,
    pub memory_mib: u64,
}

impl HostTotals {
    fn of(system: &mut System) -> Self {
        system.refresh_memory();
        Self {
            cpus: system.cpus().len().max(1),
            memory_mib: total_memory_mib(system),
        }
    }
}

fn total_memory_mib(system: &System) -> u64 {
    system.total_memory() / (1024 * 1024)
}

fn smallest_ceiling() -> SandboxCeiling {
    SandboxCeiling {
        cpus: 1,
        memory_mib: u32::try_from(MINIMUM_CEILING_MIB).unwrap_or(u32::MAX),
        disk_mib: SANDBOX_DISK_CEILING_MIB,
    }
}

// The valve, and the sizes that go with it. It holds no count of how many sandboxes
// the machine may carry, because nothing here reserves anything: it reads what the
// machine is doing and answers whether it is coping.
#[derive(Debug)]
pub struct HostPressure {
    sampler: Option<HostSampler>,
    totals: HostTotals,
    window: SampleWindow,
    data_dir: PathBuf,
    held: Option<Pressure>,
}

impl HostPressure {
    pub fn new(data_dir: &Path) -> Self {
        let mut sampler = HostSampler::new();
        let totals = sampler
            .as_mut()
            .map(HostSampler::totals)
            .unwrap_or(HostTotals {
                cpus: 1,
                memory_mib: 0,
            });
        Self {
            sampler,
            totals,
            window: SampleWindow::new(),
            data_dir: data_dir.to_owned(),
            held: None,
        }
    }

    // A reading and the valve's answer with it: they move together, and a caller that
    // sampled and then asked would be reading an answer one sample old.
    pub fn observe(&mut self) -> Option<Pressure> {
        let low_disk = self.disk_available_mib() < DISK_FLOOR_MIB;
        if let Some(sample) = self.sampler.as_mut().and_then(HostSampler::sample) {
            self.window.push(sample);
        }
        let (Some(cpu), Some(memory)) = (self.window.cpu_p75(), self.window.memory_p75()) else {
            // Too few readings to judge the machine. A valve that shut on no readings
            // would refuse the first task after every restart, so only the disk — which
            // is a fact rather than a sample — can close it this early.
            self.held = low_disk.then_some(Pressure::Disk);
            return self.held;
        };
        if self.window.is_warming_up() {
            self.held = low_disk.then_some(Pressure::Disk);
            return self.held;
        }
        let busy = if cpu > BUSY_CPU_PERCENT {
            Some(Pressure::Cpu)
        } else if memory > BUSY_MEMORY_PERCENT {
            Some(Pressure::Memory)
        } else if low_disk {
            Some(Pressure::Disk)
        } else {
            None
        };
        let calm = cpu < CALM_CPU_PERCENT && memory < CALM_MEMORY_PERCENT && !low_disk;
        self.held = match busy {
            // The reason is the current reading, so a machine that shut for cpu and is
            // now holding on memory says memory.
            Some(busy) => Some(busy),
            // A held valve opens on calm, not at the line it shut at: that is the whole
            // of the hysteresis, and the dead band between the two numbers is where it
            // stays as it was.
            None if calm => None,
            None => self.held,
        };
        self.held
    }

    pub fn pressure(&self) -> Option<Pressure> {
        self.held
    }

    // Sized from what the machine has left rather than from a count of how many it is
    // keeping: a ceiling is an upper bound, so the only honest input is memory it can
    // actually give away now. A box filling up hands out smaller sandboxes by itself.
    pub fn ceiling(&self) -> SandboxCeiling {
        if self.totals.memory_mib == 0 {
            return smallest_ceiling();
        }
        let available_mib = self
            .window
            .latest()
            .map_or(self.totals.memory_mib, |sample| sample.available_mib);
        let memory_mib = (available_mib * MEMORY_SHARE_OF_HOST / 100)
            .clamp(MINIMUM_CEILING_MIB, MAXIMUM_CEILING_MIB);
        SandboxCeiling {
            // Every guest may be told the machine's cores: vcpus are time-shared, and
            // what keeps the number of guests down is the valve, not the ceiling.
            cpus: u8::try_from(self.totals.cpus.clamp(1, usize::from(MAXIMUM_CEILING_CPUS)))
                .unwrap_or(1),
            memory_mib: u32::try_from(memory_mib.min(u64::from(u32::MAX))).unwrap_or(u32::MAX),
            disk_mib: SANDBOX_DISK_CEILING_MIB,
        }
    }

    fn disk_available_mib(&self) -> u64 {
        // The filesystem holding the sandboxes, which is the one that fills when they
        // do. Anything unreadable is treated as having room rather than as pressure:
        // a machine that cannot answer is not a machine to stop taking work for.
        let Ok(disks) = Disks::new_with_refreshed_list() else {
            return u64::MAX;
        };
        disks
            .iter()
            .filter(|disk| self.data_dir.starts_with(disk.mount_point()))
            .max_by_key(|disk| disk.mount_point().as_os_str().len())
            .map_or(u64::MAX, |disk| disk.available_space() / (1024 * 1024))
    }
}

impl Default for HostPressure {
    fn default() -> Self {
        Self::new(Path::new("."))
    }
}

#[derive(Debug)]
pub struct HostSampler {
    system: System,
    last_refresh: Option<Instant>,
}

impl HostSampler {
    pub fn new() -> Option<Self> {
        let system = System::new().ok()?;
        Some(Self {
            system,
            last_refresh: None,
        })
    }
}

impl HostSampler {
    fn totals(&mut self) -> HostTotals {
        HostTotals::of(&mut self.system)
    }

    pub fn sample(&mut self) -> Option<ResourceSample> {
        let now = Instant::now();
        if let Some(last) = self.last_refresh
            && now.duration_since(last) < MINIMUM_CPU_UPDATE_INTERVAL
        {
            return None;
        }
        self.system.refresh_memory();
        self.system.refresh_cpu_usage();
        let establishing_baseline = self.last_refresh.is_none();
        self.last_refresh = Some(now);
        if establishing_baseline {
            return None;
        }
        // Both or neither: a reading that cannot see memory is not a reading this
        // valve should act on.
        let total = self.system.total_memory();
        if total == 0 {
            return None;
        }
        let available = self.system.available_memory();
        let used = total.saturating_sub(available);
        Some(ResourceSample {
            at_ms: now_ms(),
            cpu_percent: f64::from(self.system.global_cpu_usage()).clamp(0.0, 100.0),
            memory_percent: (100.0 * used as f64 / total as f64).clamp(0.0, 100.0),
            available_mib: available / (1024 * 1024),
        })
    }
}

fn percentile_75(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut sorted: Vec<f64> = values.collect();
    sorted.sort_by(f64::total_cmp);
    let rank = (0.75 * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.checked_sub(1)?).copied()
}

fn now_ms() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or_default()
}
