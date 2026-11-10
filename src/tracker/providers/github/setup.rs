use crate::tracker::{Guide, Item, Step};

pub(crate) static GITHUB: Guide = Guide {
    title: "Set up GitHub on a remote",
    intro: &[
        "A remote acts on GitHub issues as a GitHub App, not as you. Its owner creates the app \
         once and installs it on the repositories whose issues it may work on.",
        "An issue is delegated by labelling it agent:altitude. Removing the label takes the \
         work back. Your own GitHub sign-in under Settings → Task tracker decides which machine \
         receives it.",
    ],
    steps: &[
        Step {
            title: "Choose the public URL",
            items: &[Item::Text(
                "Pick the HTTPS address that reaches this machine. It is public_url in \
                 serve.toml, and the webhook URL GitHub needs is built from it.",
            )],
        },
        Step {
            title: "Create the GitHub App",
            items: &[
                Item::Text(
                    "In GitHub's developer settings, create a GitHub App. The form asks for:",
                ),
                Item::Bullet("GitHub App name: altitude, or any name not taken."),
                Item::Bullet("Homepage URL: your own."),
                Item::Bullet("Webhook URL: public_url followed by /altitude/github/webhook."),
                Item::Bullet(
                    "Webhook secret: a long random value; copy it into webhook_secret in \
                     serve.toml.",
                ),
                Item::Bullet("Repository permissions:"),
                Item::Detail("Issues: read and write"),
                Item::Detail("Pull requests: read-only"),
                Item::Detail("Metadata: read-only"),
                Item::Bullet("Subscribe to events: Issues and Issue comment."),
                Item::Detail(
                    "Issues: the agent:altitude label being added or removed, and the issue \
                     being closed or reopened",
                ),
                Item::Detail(
                    "Issue comment: what the operator writes on the issue, which is how they \
                     talk to a task without a terminal",
                ),
                Item::Bullet("Where can this GitHub App be installed: only on this account."),
                Item::Text(
                    "After creating it, note the App ID and generate a private key. Keep the \
                     downloaded .pem on the remote, readable only by its user.",
                ),
            ],
        },
        Step {
            title: "Add the app to serve.toml",
            items: &[
                Item::Text("Put those values in [github] in the remote's serve.toml."),
                Item::Code("[github]"),
                Item::Code("app_id = \"<the App ID>\""),
                Item::Code("public_url = \"<the HTTPS address that reaches the remote>\""),
                Item::Code("private_key_path = \"<the .pem file on this machine>\""),
                Item::Code("webhook_secret = \"<the app's webhook secret>\""),
            ],
        },
        Step {
            title: "Route the webhook to the remote",
            items: &[Item::Text(
                "From public_url, route /altitude/github/webhook to this machine's webhook \
                     port — 127.0.0.1:25850 unless it was changed.",
            )],
        },
        Step {
            title: "Install the app",
            items: &[Item::Text(
                "From the app's page, install it on the repositories whose issues this remote \
                 may work on, then restart `altitude serve`.",
            )],
        },
    ],
    closing: &[
        "Then sign in here under Settings → Task tracker, choosing GitHub: that account decides \
         which machine a delegated issue runs on, and it is read from the GitHub CLI's own \
         sign-in.",
        "A remote with no app still runs work sent from altitude, and reports through each \
         operator's own sign-in.",
    ],
};
