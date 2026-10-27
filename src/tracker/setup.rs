//! The manual for installing a tracker's shared app on a remote.
//!
//! One copy of the words, read by the command that performs the setup and by the
//! client that explains what the remote's owner has to do. Every field is static,
//! so instance facts — this machine's address, port, secrets, what it installed —
//! are printed beside a guide and never inside one. That is what lets the client
//! show the same guide without carrying anything from the host it is about.

use super::TrackerKind;

pub(crate) struct Guide {
    pub title: &'static str,
    /// What the shared app is, before any step.
    pub intro: &'static [&'static str],
    pub steps: &'static [Step],
    /// What the reader does once the remote has it.
    pub closing: &'static [&'static str],
}

pub(crate) struct Step {
    pub prose: &'static str,
    /// Literal lines: toml, URLs, commands. Never formatted.
    pub code: &'static [&'static str],
}

impl TrackerKind {
    pub(crate) fn guide(self) -> &'static Guide {
        match self {
            Self::Linear => &LINEAR,
        }
    }
}

static LINEAR: Guide = Guide {
    title: "Set up Linear on a remote",
    intro: &[
        "A remote signs in as a shared Linear app, not as you. Its owner installs it once per \
         workspace, and every task delegated there acts as that app. Your own Linear sign-in \
         under Settings → Task tracker is separate: it decides which machine receives an issue \
         delegated in Linear.",
    ],
    steps: &[
        Step {
            prose: "Create an OAuth application in Linear's API settings, named Altitude. \
                    Enable webhooks, then select Agent session events (required), Inbox \
                    notifications (required for comments and mentions), and Permission changes. \
                    Select Issues too if this remote should react to human status changes; \
                    without it Altitude still reports status out, but does not notice an issue \
                    being completed or moved. Leave unrelated data and OAuth authorization \
                    events off. The install asks for read, write, app:assignable, and \
                    app:mentionable.",
            code: &[],
        },
        Step {
            prose: "Give the remote the application in its serve.toml. public_url is the HTTPS \
                    address that reaches that machine; every callback and webhook URL is built \
                    from it.",
            code: &[
                "[linear]",
                "client_id = \"<the application's client ID>\"",
                "public_url = \"<the HTTPS address that reaches the remote>\"",
                "client_secret = \"<the application's client secret>\"",
                "webhook_secret = \"<the application's webhook signing secret>\"",
            ],
        },
        Step {
            prose: "Route /linear/oauth/callback and /linear/webhook from that address to the \
                    webhook port in that machine's settings — 127.0.0.1:25849 unless it was \
                    changed — then register both as the application's redirect and webhook \
                    URLs.",
            code: &[],
        },
        Step {
            prose: "Run this on the remote. It prints the exact URLs to register, installs the \
                    app, and saves the login there, which is the only machine that holds it.",
            code: &["altitude serve connect linear"],
        },
    ],
    closing: &[
        "Then sign in here under Settings → Task tracker and sync this machine to the remote: \
         that account decides which machine a delegated issue runs on.",
        "A remote with no shared app still runs work sent from Altitude, and reports through each \
         operator's own login.",
    ],
};
