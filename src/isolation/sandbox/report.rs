use crate::{
    agent::{AgentEngines, AgentKind},
    config::Config,
    isolation::{LAYER_MARKER, environment_identity},
    project::Project,
    vcs::{self, VcsProviderKind},
};

use super::{IsolationError, SandboxPlan};

// What an image has to run once to carry a built layer, printed for whoever builds
// it. Read-only: composing the plan reads the configuration and writes nothing, so
// this can be run anywhere the worker's configuration is.
pub(crate) fn environment_report(
    config: &Config,
    agent: AgentKind,
    vcs_provider: VcsProviderKind,
) -> Result<String, IsolationError> {
    let agents = AgentEngines::new();
    let recipes = vec![
        agents.engine(agent).profile_recipe(),
        vcs::profile_recipe(vcs_provider),
    ];
    let plan = SandboxPlan::compose(config, Project::profile_build(), recipes)?;
    let mut report = String::from(
        "A host runs each layer below once, as the account its worker runs as, and then\n\
         writes the marker that says so: a host that finds the marker starts from the built\n\
         layer instead of running its steps again. Nothing here writes anything.\n",
    );
    for layer in &plan.layers {
        report.push_str(&format!(
            "\n## {name}\nmarker  {marker}\n",
            name = layer.name,
            marker = layer.snapshots_dir.join(LAYER_MARKER).display(),
        ));
        for step in layer
            .steps
            .iter()
            .filter(|step| !step.command.trim().is_empty())
        {
            report.push_str(&format!("\n### {}\n{}\n", step.label, step.command));
        }
    }
    if environment_identity().is_some() {
        report.push_str(
            "\nALTITUDE_WORKER_ENVIRONMENT is set, so these tags are this host's own: an\n\
             image that names its environment is asked for the layers it built.\n",
        );
    }
    Ok(report)
}
