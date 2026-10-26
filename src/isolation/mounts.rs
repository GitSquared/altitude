//! What an environment mounts.
//!
//! One definition, because an environment's snapshot is identified by the setup steps
//! of its composition: a list assembled anywhere else builds a second environment for
//! the same work, and the first one's build is thrown away or never reused. A task runs
//! one agent, so its environment has that agent's profile and the tools that give it an
//! identity; every profile Altitude can mount is a different list, because it is what
//! an interface says logins exist for, not a thing to build.

use crate::{
    agent::{AgentEngines, AgentKind},
    isolation::EnvironmentRecipe,
    vcs,
};

/// The profiles a task driven by `agent` mounts.
pub fn task(agents: &AgentEngines, agent: AgentKind) -> Vec<EnvironmentRecipe> {
    [
        vec![agents.engine(agent).environment_recipe()],
        vcs::environment_recipes(),
    ]
    .concat()
}

/// Every profile Altitude can mount, whichever agent a task runs.
pub fn every(agents: &AgentEngines) -> Vec<EnvironmentRecipe> {
    let mut recipes = vec![
        agents.engine(AgentKind::Pi).environment_recipe(),
        agents.engine(AgentKind::Claude).environment_recipe(),
    ];
    recipes.extend(vcs::environment_recipes());
    recipes
}
