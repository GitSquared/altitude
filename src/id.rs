//! Identifiers that people and agents can say out loud.
//!
//! A task can move between machines, so its id has to be unique without a
//! coordinator. Three words from the English BIP39 list give that room without
//! a disambiguator. A signal lives inside one task, so two words are enough; a
//! message lives inside one thread, so its position is its id.

use rand::RngExt;

use bip39::Language;

const TASK_WORDS: usize = 3;
const SIGNAL_WORDS: usize = 2;

pub fn task() -> String {
    words("task", TASK_WORDS)
}

pub fn signal() -> String {
    words("signal", SIGNAL_WORDS)
}

/// An id with the given prefix and distinct, lowercase English words.
fn words(prefix: &str, count: usize) -> String {
    let list = Language::English.word_list();
    let mut rng = rand::rng();
    let mut chosen = Vec::with_capacity(count);
    while chosen.len() < count {
        let word = list[rng.random_range(0..list.len())];
        if !chosen.contains(&word) {
            chosen.push(word);
        }
    }
    format!("{prefix}_{}", chosen.join("-"))
}
