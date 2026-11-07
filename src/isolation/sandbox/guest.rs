use std::{
    path::{Component, Path},
    time::Duration,
};

use super::plan::expand_guest_path;
use super::*;

use tokio::time;

use crate::project::Project;

use super::{CommandEvent, IsolationEngine, IsolationError, OutputStream};

pub(crate) fn opener_script() -> String {
    format!(
        "mkdir -p /usr/local/bin\ncat > {OPENER} <<'ALTITUDE_OPENER'\n{}ALTITUDE_OPENER\nchmod 0755 {OPENER}\n",
        include_str!("../assets/xdg-open.sh")
    )
}

pub(crate) fn relative_path(path: &str, allow_empty: bool) -> Result<(), IsolationError> {
    if (!allow_empty && path.is_empty())
        || Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(IsolationError::new(format!(
            "invalid persistent path: {path}"
        )));
    }
    Ok(())
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn guest_absolute(path: &str) -> Result<String, IsolationError> {
    Ok(expand_guest_path(path)?.to_string_lossy().into_owned())
}

pub(crate) fn project_scripts(project: &Project) -> Result<String, IsolationError> {
    let checkout = project
        .checkout_path()
        .map(|path| expand_guest_path(&path))
        .transpose()?
        .map(|path| path.to_string_lossy().into_owned());
    // The brief and Altitude's rules are mounted read-only, not written here: a
    // document the agent may not edit does not belong on the disk the agent owns.
    let mut script = String::new();
    if let (Some(repository), Some(checkout)) = (&project.repository, &checkout) {
        let parent = Path::new(checkout)
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_else(|| GUEST_HOME.to_owned());
        // No emptiness test: a cache mounted inside the checkout —
        // `~/altitude/target`, say — has already made the directory, and a shell
        // asked whether a directory is empty answers "yes" both when it is and
        // when it may not read it, while git, a line later, disagrees. What the
        // question stood for is whether git has started here, which git answers
        // itself: where it has, this is a task resuming into its own tree and is
        // left alone.
        script.push_str(&format!(
            r#"mkdir -p {parent} {checkout}
if ! git -C {checkout} rev-parse --git-dir >/dev/null 2>&1; then
  git -C {checkout} init -q
  if ! git -C {checkout} remote add origin {repository} \
    || ! git -C {checkout} fetch -q origin; then
    echo "altitude: the project's repository could not be fetched" >&2
    exit 1
  fi
  git -C {checkout} remote set-head origin --auto >/dev/null 2>&1 || true
  branch=$(git -C {checkout} symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null || echo origin/main)
  if ! git -C {checkout} checkout -q -f -B "${{branch#origin/}}" "$branch"; then
    echo "altitude: the project's repository could not be checked out" >&2
    exit 1
  fi
fi
"#,
            checkout = shell_quote(checkout),
            parent = shell_quote(&parent),
            repository = shell_quote(repository),
        ));
    }
    Ok(script)
}

pub(crate) async fn run_to_end(
    engine: &impl IsolationEngine,
    id: String,
    command: String,
) -> Result<(i32, String), IsolationError> {
    let (status, output, errors) = run_collecting(engine, id, command).await?;
    Ok((status, output + &errors))
}

pub(crate) async fn run_collecting(
    engine: &impl IsolationEngine,
    id: String,
    command: String,
) -> Result<(i32, String, String), IsolationError> {
    let mut session = engine.run_command(id, command).await?;
    let mut output = String::new();
    let mut errors = String::new();
    while let Some(event) = session.events.recv().await {
        match event {
            CommandEvent::Output { stream, bytes } => {
                let text = String::from_utf8_lossy(&bytes);
                match stream {
                    OutputStream::Stdout => output.push_str(&text),
                    OutputStream::Stderr => errors.push_str(&text),
                }
            }
            CommandEvent::Exited(status) => return Ok((status, output, errors)),
            CommandEvent::Failed(error) => return Err(IsolationError::new(error)),
        }
    }
    Err(IsolationError::new(
        "a command closed without an exit status",
    ))
}

pub(crate) const SETUP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

pub(crate) async fn run_setup(
    engine: &impl IsolationEngine,
    id: String,
    command: String,
) -> Result<(), IsolationError> {
    // Kept outside the command so a wait that is cut short still says what the
    // sandbox had printed: a prompt is the only trace a stuck setup leaves.
    let mut output = String::new();
    let setup = async {
        let mut session = engine.run_command(id, command).await?;
        while let Some(event) = session.events.recv().await {
            match event {
                CommandEvent::Output { bytes, .. } => {
                    output.push_str(&String::from_utf8_lossy(&bytes));
                }
                CommandEvent::Exited(0) => return Ok(()),
                CommandEvent::Exited(code) => {
                    return Err(IsolationError::new(format!(
                        "task directory setup exited with code {code}: {}",
                        printed_tail(&output),
                    )));
                }
                CommandEvent::Failed(error) => return Err(IsolationError::new(error)),
            }
        }
        Err(IsolationError::new(
            "task directory setup closed without an exit status",
        ))
    };
    let result = time::timeout(SETUP_TIMEOUT, setup).await;
    match result {
        Ok(result) => result,
        Err(_) => Err(IsolationError::new(format!(
            "task directory setup did not finish within {} minutes: {}",
            SETUP_TIMEOUT.as_secs() / 60,
            printed_tail(&output),
        ))),
    }
}

pub(crate) fn printed_tail(output: &str) -> &str {
    const LIMIT: usize = 400;
    match output.char_indices().rev().nth(LIMIT) {
        Some((index, _)) => output[index..].trim(),
        None => output.trim(),
    }
}
