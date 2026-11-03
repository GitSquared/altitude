use serde::{Deserialize, Serialize};
use sqlx::Type;

// The windows a provider meters a subscription by. Altitude names the ones it knows
// so the interface can say "5h" without learning a new word for each harness; what
// it cannot classify keeps the word "usage" rather than being dropped, because a
// refusal nobody can name is still a refusal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum AgentWindow {
    Session,
    Weekly,
    WeeklyOpus,
    WeeklySonnet,
    Overage,
    Other,
}

impl AgentWindow {
    pub fn of(name: Option<&str>) -> Self {
        match name {
            Some("five_hour" | "session") => Self::Session,
            Some("seven_day" | "weekly") => Self::Weekly,
            Some("seven_day_opus") => Self::WeeklyOpus,
            Some("seven_day_sonnet") => Self::WeeklySonnet,
            Some("overage" | "seven_day_overage_included") => Self::Overage,
            _ => Self::Other,
        }
    }

    pub fn word(&self) -> &'static str {
        match self {
            Self::Session => "5h",
            Self::Weekly => "weekly",
            Self::WeeklyOpus => "weekly Opus",
            Self::WeeklySonnet => "weekly Sonnet",
            Self::Overage => "usage credits",
            Self::Other => "usage",
        }
    }
}

// One window of an account's standing, as a harness reported it: how much of the
// allowance is gone and when it comes back. Nothing here is inferred from prose.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentWindowReading {
    pub window: AgentWindow,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clears_at_ms: Option<i64>,
}

// The account an agent runs on, as its harness last described it: which windows are
// open, how much of each is gone, and whether the harness itself says this credential
// is a plan. Claude reports this on its own stream; pi reports nothing of the kind
// without an extension of ours, and a harness that says nothing says nothing here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentStanding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    // Some only when the harness said so: a plan is priced at list rates and an API
    // key is not, and this is the fact that separates them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_plan: Option<bool>,
    pub observed_at_ms: i64,
    #[serde(default)]
    pub windows: Vec<AgentWindowReading>,
}

// What the account said when it refused the harness. A window is a clock, and a
// clock is a wait Altitude can take on the task's behalf. A stop is the operator's:
// it is money, or a limit nobody said when it clears, and neither is a thing to
// spend on their behalf by retrying.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRefusal {
    Window {
        window: AgentWindow,
        clears_at_ms: i64,
    },
    Stopped {
        said: String,
    },
}

impl AgentRefusal {
    // A window reopens on its own clock, and a task that arrives on the very second
    // it rolls can be refused by a window it has only just left: a minute of grace is
    // cheaper than a provision spent learning nothing. Every harness gets it here,
    // because a window is always made in this one place.
    const GRACE_MS: i64 = 60_000;

    pub fn window(window: AgentWindow, clears_at_ms: i64) -> Self {
        Self::Window {
            window,
            clears_at_ms: clears_at_ms.saturating_add(Self::GRACE_MS),
        }
    }

    pub fn stopped(said: impl Into<String>) -> Self {
        Self::Stopped { said: said.into() }
    }
}
