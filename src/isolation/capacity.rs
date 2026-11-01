use std::{
    cmp::Ordering,
    collections::VecDeque,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, System};

use super::SandboxCeiling;

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

pub const WINDOW: Duration = Duration::from_secs(10 * 60);

pub const CPU_TARGET_PERCENT: f64 = 75.0;

pub const MEMORY_TARGET_PERCENT: f64 = 80.0;

pub const MIN_READINGS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceSample {
    pub at_ms: i64,
    pub cpu_percent: f64,
    pub memory_percent: f64,
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

    pub fn cpu_p75(&self) -> Option<f64> {
        percentile_75(self.samples.iter().map(|sample| sample.cpu_percent))
    }

    pub fn memory_p75(&self) -> Option<f64> {
        percentile_75(self.samples.iter().map(|sample| sample.memory_percent))
    }
}

/// What the machine has to divide up, read once: it does not change under us.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostTotals {
    pub cpus: usize,
    pub memory_mib: u64,
}

/// The same machine's own numbers as sysinfo reports them.
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

/// What a sandbox may be given on this host, and how many may run at once.
///
/// Both answers come from the same two numbers — the machine's totals and the
/// count the policy has settled on — so the worst case is bounded by the last
/// ceiling handed out times the count in force, and neither can drift from the
/// other. A defaulted value means this host could not be read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SandboxCapacity {
    pub slots: usize,
    pub ceiling: SandboxCeiling,
}

impl SandboxCapacity {
    pub fn free_slots_after(&self, in_use: usize) -> usize {
        self.slots.saturating_sub(in_use)
    }
}

impl Default for SandboxCapacity {
    fn default() -> Self {
        Self {
            slots: 1,
            ceiling: smallest_ceiling(),
        }
    }
}

/// The ceiling a host that cannot be read still gets.
///
/// Small rather than generous: a machine whose own numbers are unavailable is
/// not one to hand a large guest to, and the floor is what can still run an
/// agent at all.
fn smallest_ceiling() -> SandboxCeiling {
    SandboxCeiling {
        cpus: 1,
        memory_mib: u32::try_from(MINIMUM_CEILING_MIB).unwrap_or(u32::MAX),
        disk_mib: SANDBOX_DISK_CEILING_MIB,
    }
}

/// What one sandbox's own disk may grow to.
///
/// Not divided by the count like the others: a capsule outlives the slot it ran
/// in, so the number of them is not the number of tasks running now.
pub const SANDBOX_DISK_CEILING_MIB: u32 = 15 * 1_024;

pub const MEMORY_SHARE_OF_HOST: u64 = 80;
pub const MINIMUM_CEILING_MIB: u64 = 2 * 1_024;
pub const MAXIMUM_CEILING_MIB: u64 = 8 * 1_024;
pub const MAXIMUM_CEILING_CPUS: u8 = 4;

pub fn optimistic_start() -> usize {
    thread::available_parallelism().map_or(1, |cores| cores.get())
}

pub fn next_slot_limit(current: usize, window: &SampleWindow, demand: usize) -> usize {
    let (Some(cpu), Some(memory)) = (window.cpu_p75(), window.memory_p75()) else {
        return current;
    };
    if window.is_warming_up() {
        return current;
    }
    if cpu > CPU_TARGET_PERCENT || memory > MEMORY_TARGET_PERCENT {
        return current.saturating_sub(1).max(1);
    }
    match demand.cmp(&current) {
        Ordering::Greater => current + 1,
        Ordering::Less => current.saturating_sub(1).max(1),
        Ordering::Equal => current,
    }
}

fn percentile_75(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut sorted: Vec<f64> = values.collect();
    sorted.sort_by(f64::total_cmp);
    let rank = (0.75 * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.checked_sub(1)?).copied()
}

/// How many sandboxes this host will run at once, and why.
///
/// The sampling belongs here with the rule: a host that cannot read itself is
/// not an empty host, and that judgement has no business in the task queue.
#[derive(Debug)]
pub struct HostCapacity {
    sampler: Option<HostSampler>,
    totals: HostTotals,
    window: SampleWindow,
    slots: usize,
}

impl HostCapacity {
    pub fn new() -> Self {
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
            slots: optimistic_start(),
        }
    }

    /// What one sandbox may be given, dividing the host by the count in force.
    ///
    /// Dividing by the count is what keeps the ceilings and the number of
    /// sandboxes from disagreeing: letting another sandbox run also narrows
    /// every later one, without touching a sandbox already running.
    fn ceiling(&self) -> SandboxCeiling {
        if self.totals.memory_mib == 0 {
            return smallest_ceiling();
        }
        let share = u64::try_from(self.slots.max(1)).unwrap_or(u64::MAX);
        // The floor wins over the share, deliberately: a sandbox too small to run
        // a package manager fails in a way that looks like a broken agent, while
        // one that grows past its share is caught by the sampler, which pulls the
        // count down rather than shrinking work already in progress. The ceiling
        // is a bound, not a reservation, so the overshoot costs nothing until a
        // guest actually touches that memory.
        let memory_mib = (self.totals.memory_mib * MEMORY_SHARE_OF_HOST / 100 / share)
            .clamp(MINIMUM_CEILING_MIB, MAXIMUM_CEILING_MIB);
        let cpus = u64::try_from(self.totals.cpus).unwrap_or(1) / share;
        SandboxCeiling {
            cpus: u8::try_from(cpus.clamp(1, u64::from(MAXIMUM_CEILING_CPUS))).unwrap_or(1),
            memory_mib: u32::try_from(memory_mib.min(u64::from(u32::MAX))).unwrap_or(u32::MAX),
            disk_mib: SANDBOX_DISK_CEILING_MIB,
        }
    }

    pub fn sandbox_capacity(&self) -> SandboxCapacity {
        SandboxCapacity {
            slots: self.slots,
            ceiling: self.ceiling(),
        }
    }

    /// Re-measure the host and let the policy move the count.
    ///
    /// Returns `None` when this host cannot be read, which leaves the count
    /// where it is: an unreadable machine is not an empty one.
    pub fn observe(&mut self, demand: usize) -> Option<usize> {
        let sample = self.sampler.as_mut()?.sample()?;
        self.window.push(sample);
        self.slots = next_slot_limit(self.slots, &self.window, demand);
        Some(self.slots)
    }
}

impl Default for HostCapacity {
    fn default() -> Self {
        Self::new()
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
        // rule should act on.
        let total = self.system.total_memory();
        if total == 0 {
            return None;
        }
        let used = total.saturating_sub(self.system.available_memory());
        Some(ResourceSample {
            at_ms: now_ms(),
            cpu_percent: f64::from(self.system.global_cpu_usage()).clamp(0.0, 100.0),
            memory_percent: (100.0 * used as f64 / total as f64).clamp(0.0, 100.0),
        })
    }
}

fn now_ms() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or_default()
}
