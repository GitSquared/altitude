//! How many tasks this host will run at once.
//!
//! A queue with a visible reason, against a limit computed from the host rather
//! than configured by hand.
//!
//! Three separate things, none of which holds the limit itself:
//!
//! - [`SampleWindow`] is the host readings, and nothing else.
//! - [`next_slot_limit`] is the rule that turns readings and demand into a limit.
//! - [`HostSampler`] is where the readings come from.
//!
//! The caller owns the current limit, including the value it starts at. That is
//! deliberate: the starting value is a product decision, not a property of the
//! rule, and [`optimistic_start`] is the recommended one.
//!
//! Nothing here is bounded by the CPU and memory a capsule is created with.
//! Those are ceilings, not pre-allocations — most tasks use a fraction of them
//! most of the time — so a limit derived from them would cap a host far below
//! what it can actually carry. Observation decides instead.

use std::{
    cmp::Ordering,
    collections::VecDeque,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, System};

/// How often to sample. Nothing here enforces it; whoever drives [`HostSampler`]
/// sets the cadence.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

/// How far back the rule looks. A spike should not close a slot; sustained
/// pressure should.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);

/// CPU the host must be under across the window before the limit grows. The gap
/// to 100% is headroom for the host's own work and for bursts.
pub const CPU_TARGET_PERCENT: f64 = 75.0;

/// Memory the host must be under across the window before the limit grows.
/// Higher than the CPU target: memory is not reclaimed as quickly, and a capsule
/// that runs out of it fails rather than slows down.
pub const MEMORY_TARGET_PERCENT: f64 = 80.0;

/// The fewest readings at which the window says anything worth acting on.
///
/// Four is not a taste call: it is the smallest count at which a nearest-rank
/// 75th percentile is a 75th percentile. Below that the same arithmetic returns
/// the maximum of what it was given, which is both a different statistic and one
/// that moves the wrong way as the window fills. So the rule holds still until
/// the window has four, which is about forty seconds at [`SAMPLE_INTERVAL`].
pub const MIN_READINGS: usize = 4;

/// One reading of the whole host. Both percentages are in `0.0..=100.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceSample {
    /// Milliseconds since the Unix epoch, matching the timestamps tasks record.
    pub at_ms: i64,
    pub cpu_percent: f64,
    pub memory_percent: f64,
}

/// The host readings still inside the window.
///
/// Holds no opinion: it keeps what it was given and drops what aged out, so the
/// limit and the reason for it can be read separately.
#[derive(Debug, Default)]
pub struct SampleWindow {
    samples: VecDeque<ResourceSample>,
}

impl SampleWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep a reading and forget anything that has left the window.
    ///
    /// Readings must arrive in nondecreasing `at_ms` order, as a sampler on an
    /// interval produces; the newest reading is the clock the window is measured
    /// back from.
    pub fn push(&mut self, sample: ResourceSample) {
        let window_ms = i64::try_from(WINDOW.as_millis()).unwrap_or(i64::MAX);
        let cutoff = sample.at_ms.saturating_sub(window_ms);
        while self.samples.front().is_some_and(|old| old.at_ms < cutoff) {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// Whether the window is still too young to act on. See [`MIN_READINGS`].
    pub fn is_warming_up(&self) -> bool {
        self.samples.len() < MIN_READINGS
    }

    /// The 75th percentile of CPU over the window, or `None` while it is empty.
    pub fn cpu_p75(&self) -> Option<f64> {
        percentile_75(self.samples.iter().map(|sample| sample.cpu_percent))
    }

    /// The 75th percentile of memory over the window, or `None` while it is empty.
    pub fn memory_p75(&self) -> Option<f64> {
        percentile_75(self.samples.iter().map(|sample| sample.memory_percent))
    }
}

/// How many slots the host may start with, before anything has been observed.
///
/// One task per core. Optimistic on purpose: the caps a capsule is created with
/// are ceilings rather than reservations, so a core each is a defensible guess to
/// begin from, and observation corrects it within a minute.
pub fn optimistic_start() -> usize {
    thread::available_parallelism().map_or(1, |cores| cores.get())
}

/// How many slots the host should allow, given the window and the demand.
///
/// `demand` is the tasks wanting a slot, running or waiting. While the window is
/// warming up the limit holds: capacity is already at its optimistic start, and
/// a window of one to three readings is not evidence for moving it.
///
/// Once warm, over either target shrinks by one; otherwise demand above the limit
/// grows by one and demand below it shrinks by one. Never below one, and never by
/// more than one per reading.
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

/// The 75th percentile by nearest rank: index `ceil(0.75 * n) - 1` of the sorted
/// readings, with no interpolation. Matches the prototype this came from, so the
/// two are comparable.
fn percentile_75(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut sorted: Vec<f64> = values.collect();
    sorted.sort_by(f64::total_cmp);
    let rank = (0.75 * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.checked_sub(1)?).copied()
}

/// The host's capacity: what it has observed, and the limit that follows.
///
/// Carries the limit so a caller does not have to, and nothing else. It is not a
/// queue and knows nothing about tasks; deciding who runs is the caller's job.
#[derive(Debug)]
pub struct HostCapacity {
    window: SampleWindow,
    limit: usize,
}

impl HostCapacity {
    /// Capacity before anything has been observed: see [`optimistic_start`].
    pub fn new() -> Self {
        Self {
            window: SampleWindow::new(),
            limit: optimistic_start(),
        }
    }

    /// Take a reading and report the limit that now applies.
    pub fn observe(&mut self, sample: ResourceSample, demand: usize) -> usize {
        self.window.push(sample);
        self.limit = next_slot_limit(self.limit, &self.window, demand);
        self.limit
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
}

impl Default for HostCapacity {
    fn default() -> Self {
        Self::new()
    }
}

/// This host, through `sysinfo`.
///
/// Stateful because CPU utilisation is a difference between two refreshes, and
/// the previous refresh is the baseline.
#[derive(Debug)]
pub struct HostSampler {
    system: System,
    last_refresh: Option<Instant>,
}

impl HostSampler {
    pub fn new() -> Self {
        Self {
            system: System::new(),
            last_refresh: None,
        }
    }
}

impl Default for HostSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl HostSampler {
    /// A fresh reading, or `None` when one is not available yet.
    ///
    /// `None` is not an error. CPU utilisation is a difference between two
    /// refreshes, so the first call has nothing to compare against, and a call
    /// arriving sooner than the platform's own minimum interval would be noise
    /// rather than measurement.
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
