use std::mem;

use crate::isolation::PackageInstall;

// What an install is doing, read out of the lines dpkg writes: it names each package
// as it configures it ("Setting up jq (1.7.1-3build2) ..."). The count only grows,
// and the name it last printed is the one being configured now.
#[derive(Default)]
pub(super) struct Packages {
    // The last line is held back until it is whole: a chunk can end mid-line, and
    // half a package name is not a package.
    pending: String,
    install: PackageInstall,
}

impl Packages {
    // Reads a chunk of output and answers the count if this chunk moved it.
    pub(super) fn observe(&mut self, chunk: &str) -> Option<PackageInstall> {
        self.pending.push_str(chunk);
        let mut moved = false;
        while let Some(end) = self.pending.find('\n') {
            let line = self.pending[..end].to_owned();
            self.pending.drain(..=end);
            moved |= self.read(&line);
        }
        // A process that writes without newlines still has to be read: a line long
        // enough to be complete is one, whether or not its newline has arrived.
        if self.pending.len() > 4096 {
            let line = mem::take(&mut self.pending);
            moved |= self.read(&line);
        }
        moved.then(|| self.install.clone())
    }

    fn read(&mut self, line: &str) -> bool {
        let Some(rest) = line.trim().strip_prefix("Setting up ") else {
            return false;
        };
        // "Setting up jq (1.7.1-3build2) ...": the name is the first word.
        let Some(name) = rest.split_whitespace().next() else {
            return false;
        };
        self.install.installed += 1;
        self.install.current = Some(name.to_owned());
        true
    }
}
