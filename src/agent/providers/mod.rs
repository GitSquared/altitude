mod claude;
mod pi;

pub use claude::ClaudeCode;
pub use pi::PiCode;

use super::AgentKind;

impl AgentKind {
    pub fn profile(self) -> &'static str {
        match self {
            Self::Claude => "agents/claude",
            Self::Pi => "agents/pi",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Pi => "Pi",
        }
    }

    pub fn purpose(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code uses your account to run tasks.",
            Self::Pi => "Pi uses your provider account to run tasks.",
        }
    }

    pub fn login_files(self) -> &'static [&'static str] {
        match self {
            // Claude Code's own login file; its settings and MCP files stay.
            Self::Claude => &[".credentials.json"],
            // Pi keeps one credential per provider here, and nothing else.
            Self::Pi => &["auth.json"],
        }
    }
}
