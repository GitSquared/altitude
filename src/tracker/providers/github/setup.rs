use crate::tracker::{Guide, Item, Step};

pub(crate) static GITHUB: Guide = Guide {
    title: "GitHub on a remote",
    intro: &[
        "A remote cannot take work delegated from GitHub yet. GitHub works from this machine: \
         start a task from an issue, and altitude moves its status labels as it goes.",
    ],
    steps: &[Step {
        title: "Use GitHub from this machine",
        items: &[Item::Text(
            "Sign in under Settings → Task tracker, choosing GitHub. altitude reads the GitHub \
             CLI's own login, so run `gh auth login` first if it is not signed in.",
        )],
    }],
    closing: &[],
};
