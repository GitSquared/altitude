use crate::tracker::{Guide, Item, Step, TrackerKind};

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
                Item::Bullet("Application name: altitude."),
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
                Item::Detail("Inbox notifications (required for mentions)"),
                Item::Detail("Permission changes"),
                Item::Bullet(
                    "Data change events: select Issues, Comments and Emoji reactions if this \
                     remote should hear what people do on the issue.",
                ),
                Item::Detail(
                    "Issues: without it altitude still reports status out, but does not notice \
                     an issue being completed or moved",
                ),
                Item::Detail(
                    "Comments: what the operator writes on the issue, which is the only way \
                     they can talk to a task without opening the session",
                ),
                Item::Detail(
                    "Emoji reactions: an answer given with a symbol, carried to the thread \
                     as what happened and read by the agent in context",
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
        "A remote with no shared app still runs work sent from altitude, and reports through each \
         operator's own login.",
    ],
};
