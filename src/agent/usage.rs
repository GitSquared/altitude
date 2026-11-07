use serde::{Deserialize, Serialize};

// Tokens as a harness counted them, split the ways an energy estimate has to treat
// separately: a cache write re-runs the whole prefill, a cache read reuses it, and
// an output token is decoded one at a time against the whole context. The harness's
// numbers, like the cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTokens {
    #[serde(default)]
    pub input: i64,
    #[serde(default)]
    pub cache_write: i64,
    #[serde(default)]
    pub cache_read: i64,
    #[serde(default)]
    pub output: i64,
}

impl TaskTokens {
    pub fn is_empty(&self) -> bool {
        self.input == 0 && self.cache_write == 0 && self.cache_read == 0 && self.output == 0
    }
}

// What a run cost, as the harness counted it. Altitude records the harness's number
// and never works one out for itself: which model was used, at what rate, and whether
// the price came from the provider or from the harness's own table are all things the
// harness knows and this does not.
//
// The tokens are the harness's counts too. The footprint is not: no harness reports
// one, so `super::carbon` derives it from these tokens and the model, and says so.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskUsage {
    // Micro-dollars, because a turn can cost a fraction of a cent and a float in a
    // running total drifts.
    #[serde(default)]
    pub cost_usd_micros: i64,
    // Whether the provider priced the run itself, rather than the harness pricing it
    // from its own table. An estimate is shown as one.
    #[serde(default)]
    pub cost_from_provider: bool,
    // Whether this money is money: a harness prices a plan's turns at list rates
    // whether or not anything is being paid, so a run the account is not charged for
    // carries a number that is only what it would have cost.
    #[serde(default)]
    pub is_charged: bool,
    #[serde(default)]
    pub tokens: TaskTokens,
    // Micro-grams of CO2e, the same way cost is micro-dollars, and zero when no
    // factor covers the model. Kept beside the tokens it came from so a factor can be
    // revised without having lost the evidence.
    #[serde(default)]
    pub co2e_micrograms: i64,
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
    pub cost_from_provider: bool,
    pub is_charged: bool,
    pub tokens: TaskTokens,
    // Derived from the tokens and the run's model when the run is recorded, not by
    // the harness.
    pub co2e_micrograms: i64,
}

impl AgentUsage {
    pub fn is_unpriced(&self) -> bool {
        self.cost_usd_micros == 0
    }

    pub fn is_unused(&self) -> bool {
        self.is_unpriced() && self.tokens.is_empty()
    }

    // A run resumed from a conversation the harness counts cumulatively can report a
    // smaller total than the one already recorded; subtracting there would take back
    // what an earlier run spent. Tokens are cumulative the same way.
    pub fn since(&self, previous: &TaskUsage) -> Self {
        Self {
            cost_usd_micros: (self.cost_usd_micros - previous.cost_usd_micros).max(0),
            cost_from_provider: self.cost_from_provider,
            is_charged: self.is_charged,
            tokens: TaskTokens {
                input: (self.tokens.input - previous.tokens.input).max(0),
                cache_write: (self.tokens.cache_write - previous.tokens.cache_write).max(0),
                cache_read: (self.tokens.cache_read - previous.tokens.cache_read).max(0),
                output: (self.tokens.output - previous.tokens.output).max(0),
            },
            co2e_micrograms: 0,
        }
    }
}
