use std::{
    cmp::Ordering,
    collections::VecDeque,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, System};

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

#[derive(Debug)]
pub struct HostCapacity {
    window: SampleWindow,
    limit: usize,
}

impl HostCapacity {
    pub fn new() -> Self {
        Self {
            window: SampleWindow::new(),
            limit: optimistic_start(),
        }
    }

    pub fn observe(&mut self, sample: ResourceSample, demand: usize) -> usize {
        self.window.push(sample);
        self.limit = next_slot_limit(self.limit, &self.window, demand);
        self.limit
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// How many more sandboxes this host will take, given how many hold one.
    ///
    /// The task queue asks this instead of subtracting a limit it would have to
    /// know the meaning of: what fits is the host's business.
    pub fn free_slots(&self, in_use: usize) -> usize {
        self.limit.saturating_sub(in_use)
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
