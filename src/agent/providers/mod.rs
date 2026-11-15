mod claude;

pub use claude::ClaudeCode;

use super::AgentKind;

impl AgentKind {
    pub fn profile(self) -> &'static str {
        match self {
            Self::Claude => "agents/claude",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
        }
    }

    pub fn purpose(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code uses your account to run tasks.",
        }
    }

    pub fn login_files(self) -> &'static [&'static str] {
        match self {
            // Claude Code's own login file; its settings and MCP files stay.
            Self::Claude => &[".credentials.json"],
        }
    }
}
