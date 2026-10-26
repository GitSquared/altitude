//! The profiles a task mounts.
//!
//! One definition, because a build is identified by the setup steps of its
//! composition: a list assembled anywhere else builds a second machine for the same
//! work, and the first one's build is thrown away or never reused. A task runs one
//! agent, so its build carries that agent's profile and the tools that give it an
//! identity; every profile Altitude can mount is a different list, because it is what
//! an interface says logins exist for, not a thing to build.

use crate::{
    agent::{AgentEngines, AgentKind},
    isolation::ProfileRecipe,
    vcs,
};

/// The profiles a task driven by `agent` mounts.
pub fn task(agents: &AgentEngines, agent: AgentKind) -> Vec<ProfileRecipe> {
    [
        vec![agents.engine(agent).profile_recipe()],
        vcs::profile_recipes(),
    ]
    .concat()
}

/// Every profile Altitude can mount, whichever agent a task runs.
pub fn every(agents: &AgentEngines) -> Vec<ProfileRecipe> {
    let mut recipes = vec![
        agents.engine(AgentKind::Pi).profile_recipe(),
        agents.engine(AgentKind::Claude).profile_recipe(),
    ];
    recipes.extend(vcs::profile_recipes());
    recipes
}
