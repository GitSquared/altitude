use crate::{
    agent::{AGENTS, AgentEngines, AgentKind},
    isolation::ProfileRecipe,
    vcs,
};

pub fn task(agents: &AgentEngines, agent: AgentKind) -> Vec<ProfileRecipe> {
    [
        vec![agents.engine(agent).profile_recipe()],
        vcs::profile_recipes(),
    ]
    .concat()
}

pub fn every(agents: &AgentEngines) -> Vec<ProfileRecipe> {
    let mut recipes: Vec<ProfileRecipe> = AGENTS
        .into_iter()
        .map(|kind| agents.engine(kind).profile_recipe())
        .collect();
    recipes.extend(vcs::profile_recipes());
    recipes
}
