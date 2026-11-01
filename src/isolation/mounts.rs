use crate::{
    agent::{AgentEngines, AgentKind},
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
    let mut recipes = vec![
        agents.engine(AgentKind::Pi).profile_recipe(),
        agents.engine(AgentKind::Claude).profile_recipe(),
    ];
    recipes.extend(vcs::profile_recipes());
    recipes
}
