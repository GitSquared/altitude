use serde::{Deserialize, Serialize};

// What a run cost, as the harness counted it. Altitude records the harness's number
// and never works one out for itself: which model was used, at what rate, and whether
// the price came from the provider or from the harness's own table are all things the
// harness knows and this does not.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskUsage {
    // Micro-dollars, because a turn can cost a fraction of a cent and a float in a
    // running total drifts.
    #[serde(default)]
    pub cost_usd_micros: i64,
    #[serde(default)]
    pub total_tokens: i64,
    // Whether the provider priced the run itself, rather than the harness pricing it
    // from its own table. An estimate is shown as one.
    #[serde(default)]
    pub cost_from_provider: bool,
}

impl TaskUsage {
    pub fn is_unpriced(&self) -> bool {
        self.cost_usd_micros == 0
    }
}

// One run's usage. The decoders hand over a delta whatever their harness counts in,
// so the task's total is always a sum and never a value that has to be read twice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentUsage {
    pub cost_usd_micros: i64,
    pub total_tokens: i64,
    pub cost_from_provider: bool,
}

impl AgentUsage {
    pub fn is_empty(&self) -> bool {
        self.cost_usd_micros == 0 && self.total_tokens == 0
    }

    // A run resumed from a conversation the harness counts cumulatively can report a
    // smaller total than the one already recorded; subtracting there would take back
    // what an earlier run spent.
    pub fn since(&self, previous: &TaskUsage) -> Self {
        Self {
            cost_usd_micros: (self.cost_usd_micros - previous.cost_usd_micros).max(0),
            total_tokens: (self.total_tokens - previous.total_tokens).max(0),
            cost_from_provider: self.cost_from_provider,
        }
    }
}
