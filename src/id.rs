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

pub fn name(id: &str) -> &str {
    id.split_once('_').map_or(id, |(_, name)| name)
}

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
