use std::collections::VecDeque;

use crate::isolation::IsolationEngine;

use crate::isolation::Command;
use crate::task::TaskEventLoop;

#[derive(Default)]
pub(crate) struct CachedTerminalOutput {
    pub(super) chunks: VecDeque<Vec<u8>>,
    byte_count: usize,
}

impl CachedTerminalOutput {
    pub(super) fn push(&mut self, bytes: &[u8]) {
        const LIMIT: usize = 2 * 1024 * 1024;
        let bytes = &bytes[bytes.len().saturating_sub(LIMIT)..];
        self.byte_count += bytes.len();
        self.chunks.push_back(bytes.to_vec());
        while self.byte_count > LIMIT {
            self.byte_count -= self
                .chunks
                .pop_front()
                .expect("nonempty output cache")
                .len();
        }
    }
}

impl<E: IsolationEngine + Clone + 'static> TaskEventLoop<E> {
    pub fn cached_terminal_output(&self, task_id: &str) -> Vec<Vec<u8>> {
        self.terminal_output
            .lock()
            .expect("terminal output lock poisoned")
            .get(task_id)
            .map(|output| output.chunks.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn cache_terminal_output(&self, task_id: &str, bytes: &[u8]) {
        self.terminal_output
            .lock()
            .expect("terminal output lock poisoned")
            .entry(task_id.to_owned())
            .or_default()
            .push(bytes);
    }

    pub fn send_terminal_input(&self, task_id: &str, bytes: Vec<u8>) {
        if let Some(run) = self
            .runs
            .lock()
            .expect("task run lock poisoned")
            .get(task_id)
        {
            let _ = run.commands.send(Command::Input(bytes));
        }
    }

    pub fn resize_terminal(&self, task_id: &str, rows: u16, cols: u16) {
        if let Some(run) = self
            .runs
            .lock()
            .expect("task run lock poisoned")
            .get(task_id)
        {
            let _ = run.commands.send(Command::Resize { rows, cols });
        }
    }
}
