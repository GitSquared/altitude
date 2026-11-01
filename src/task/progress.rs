use crate::isolation::{EnvironmentProgress, EnvironmentProgressSink};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

#[derive(Clone, Default)]
pub struct EnvironmentProgressRegistry(Arc<Mutex<HashMap<String, EnvironmentProgress>>>);

impl EnvironmentProgressRegistry {
    pub fn writer(&self, task_id: &str) -> EnvironmentProgressWriter {
        EnvironmentProgressWriter {
            task_id: task_id.to_owned(),
            registry: self.clone(),
        }
    }

    pub fn published(&self) -> HashMap<String, EnvironmentProgress> {
        self.lock().clone()
    }

    pub fn clear(&self, task_id: &str) {
        self.lock().remove(task_id);
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, EnvironmentProgress>> {
        self.0.lock().expect("environment progress lock poisoned")
    }
}

pub struct EnvironmentProgressWriter {
    task_id: String,
    registry: EnvironmentProgressRegistry,
}

impl EnvironmentProgressSink for EnvironmentProgressWriter {
    fn report(&self, progress: EnvironmentProgress) {
        self.registry.lock().insert(self.task_id.clone(), progress);
    }
}
