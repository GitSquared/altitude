use std::env;

mod microsandbox;
mod testbed;

pub use microsandbox::MicrosandboxIsolation;
pub use testbed::TestbedIsolation;

// Which machine a sandbox is. Altitude normally runs one, the microVM, and the
// testbed is for the case where it cannot: a sandbox has no `/dev/kvm`, so an
// Altitude running inside one has nothing to boot a nested microVM against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsolationBackend {
    Microsandbox,
    Testbed,
}

impl IsolationBackend {
    pub fn from_environment() -> Self {
        match env::var("ALTITUDE_ISOLATION").as_deref() {
            Ok("testbed") => Self::Testbed,
            _ => Self::Microsandbox,
        }
    }
}
