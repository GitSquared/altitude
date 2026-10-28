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
    pub title: &'static str,
    pub items: &'static [Item],
}

/// One line of a step, at the depth it is read at.
pub(crate) enum Item {
    /// A sentence under the step's title.
    Text(&'static str),
    /// A point at the step's own level.
    Bullet(&'static str),
    /// A point under the bullet above it.
    Detail(&'static str),
    /// A literal line: toml, a URL, a command. Never formatted.
    Code(&'static str),
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
         workspace, and every task delegated there acts as that app.",
        "Your own Linear sign-in under Settings → Task tracker is separate from the remote's: \
         it decides which machine receives an issue delegated in Linear.",
    ],
    steps: &[
        Step {
            title: "Choose the public URL",
            items: &[Item::Text(
                "Pick the HTTPS address that reaches this machine. It is public_url in \
                     serve.toml, and the two URLs Linear needs are built from it.",
            )],
        },
        Step {
            title: "Create the Linear application",
            items: &[
                Item::Text(
                    "In Linear's API settings, create an OAuth application. The form asks for:",
                ),
                Item::Bullet("Application icon: download this image and upload it."),
                Item::Code(
                    "https://raw.githubusercontent.com/GitSquared/altitude/main/assets/icon.png",
                ),
                Item::Bullet("Application name: Altitude."),
                Item::Bullet("Developer name and URL: your own."),
                Item::Bullet(
                    "Redirect URIs: public_url followed by /altitude/linear/oauth/callback. \
                     This command reports it as redirect URI.",
                ),
                Item::Bullet("Webhooks: on."),
                Item::Bullet(
                    "Webhook URL: public_url followed by /altitude/linear/webhook. This \
                     command reports it as webhook URL.",
                ),
                Item::Bullet("Webhook signing secret: copy it into webhook_secret in serve.toml."),
                Item::Bullet("App events: select these three."),
                Item::Detail("Agent session events (required)"),
                Item::Detail("Inbox notifications (required for comments and mentions)"),
                Item::Detail("Permission changes"),
                Item::Bullet(
                    "Data change events: select Issues if this remote should react to human \
                     status changes.",
                ),
                Item::Detail(
                    "Without it Altitude still reports status out, but does not notice an \
                     issue being completed or moved",
                ),
                Item::Bullet("Authorization events: leave OAuth authorization events off."),
                Item::Text(
                    "After creating it, copy the client ID and client secret into serve.toml. \
                     The install asks for read, write, app:assignable, and app:mentionable.",
                ),
            ],
        },
        Step {
            title: "Add the application to serve.toml",
            items: &[
                Item::Text(
                    "Put those values in [linear] in the remote's serve.toml. public_url is the \
                     address from step 1.",
                ),
                Item::Code("[linear]"),
                Item::Code("client_id = \"<the application's client ID>\""),
                Item::Code("public_url = \"<the HTTPS address that reaches the remote>\""),
                Item::Code("client_secret = \"<the application's client secret>\""),
                Item::Code("webhook_secret = \"<the application's webhook signing secret>\""),
            ],
        },
        Step {
            title: "Route the two URLs to the remote",
            items: &[
                Item::Text(
                    "From public_url, route these paths to this machine's webhook port — \
                     127.0.0.1:25849 unless it was changed:",
                ),
                Item::Bullet("/altitude/linear/oauth/callback, the redirect URI"),
                Item::Bullet("/altitude/linear/webhook, the webhook URL"),
            ],
        },
        Step {
            title: "Install it on the remote",
            items: &[
                Item::Text(
                    "Run this on the remote. It prints the URLs this machine expects, installs \
                     the app, and saves the login there, which is the only machine that holds it.",
                ),
                Item::Code("altitude serve connect linear"),
            ],
        },
    ],
    closing: &[
        "Then sign in here under Settings → Task tracker, choosing Linear: that account decides \
         which machine a delegated issue runs on.",
        "A remote with no shared app still runs work sent from Altitude, and reports through each \
         operator's own login.",
    ],
};
