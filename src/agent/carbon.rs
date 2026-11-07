use super::usage::TaskTokens;

// What a run's tokens are estimated to emit, in micro-grams of CO2e. No harness
// reports this the way it reports cost, so Altitude derives it, and the factors are
// the method rather than a measurement.
//
// Grams of CO2e per million tokens for the Claude families, from Jegham et al. 2025
// (arXiv:2505.09598) as the Claude-Carbon plugin reads it: an ordinary-least-squares
// fit of Claude 3.7 Sonnet's three measured per-query energies, converted at an AWS
// grid intensity of 0.287 kgCO2e/kWh with a PUE of 1.14. Opus is 2x Sonnet, Fable
// 2x Opus and Haiku 0.25x Sonnet, each an extrapolation because none of them was
// measured. A cache write is a full prefill and counts at the input factor; a cache
// read is a reuse and counts at a fraction of it.
//
// Order-of-magnitude only: inference alone, no embodied hardware or training, and
// the non-Sonnet families are scaled. A model with no published factor is left
// unestimated rather than given a number nobody published.
const CACHE_READ_FRACTION: f64 = 0.08;

struct FamilyFactors {
    input: f64,
    output: f64,
}

const SONNET: FamilyFactors = FamilyFactors {
    input: 39.0,
    output: 826.0,
};
const OPUS: FamilyFactors = FamilyFactors {
    input: 78.0,
    output: 1652.0,
};
const HAIKU: FamilyFactors = FamilyFactors {
    input: 10.0,
    output: 207.0,
};
const FABLE: FamilyFactors = FamilyFactors {
    input: 156.0,
    output: 3304.0,
};

fn family_of(model: &str) -> Option<&'static FamilyFactors> {
    let model = model.to_ascii_lowercase();
    // A model string that is not a Claude one may be served on hardware nothing
    // here describes, so it gets no factor rather than the Sonnet one by default.
    if !model.contains("claude") {
        return None;
    }
    if model.contains("haiku") {
        Some(&HAIKU)
    } else if model.contains("opus") {
        Some(&OPUS)
    } else if model.contains("fable") || model.contains("mythos") {
        Some(&FABLE)
    } else {
        Some(&SONNET)
    }
}

// Micro-grams, so a whole task's footprint is an integer sum like its cost. A factor
// is grams per million tokens, which is micro-grams per token, so the factors and the
// counts multiply straight into the unit wanted.
pub fn estimate_micrograms(model: Option<&str>, tokens: &TaskTokens) -> i64 {
    let Some(factors) = model.and_then(family_of) else {
        return 0;
    };
    let prefilled = tokens.input + tokens.cache_write;
    let micro_grams = prefilled as f64 * factors.input
        + tokens.cache_read as f64 * factors.input * CACHE_READ_FRACTION
        + tokens.output as f64 * factors.output;
    micro_grams.round() as i64
}
