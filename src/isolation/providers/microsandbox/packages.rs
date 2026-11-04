use std::mem;

use crate::isolation::PackageInstall;

// What an install is doing, read out of the line apt and dpkg write: apt counts
// the packages it is about to add before it starts ("0 upgraded, 34 newly
// installed"), and dpkg names each one as it configures it ("Setting up jq
// (1.7.1-3build2) ..."). A build runs as many installs as it likes, so the
// announcements add up and the names are counted against them.
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
        moved.then_some(self.install)
    }

    fn read(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.starts_with("Setting up ") {
            self.install.installed += 1;
            return true;
        }
        let Some(rest) = line.rsplit_once(" newly installed").map(|(rest, _)| rest) else {
            return false;
        };
        let count = rest
            .rsplit(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|count| count.parse::<u64>().ok());
        match count {
            Some(count) => {
                self.install.total += count;
                true
            }
            None => false,
        }
    }
}
