# {{project}} project memory

Guest: `{{guest_path}}`
Host: `{{host_path}}`

This directory is the memory of the **{{project}}** project: markdown files
shared by every task in the project, whichever machine runs it. The project's own
context — its repository, setup, caches, and the operator's instructions — is the
brief at `/altitude/project.md`, and is not repeated here.

Read the memory before you start work, and write what a later task should not
have to rediscover — a convention, the command that works, the pitfall you hit.
You choose the files and how they are shaped; plain markdown is what a person and
an agent read best. `QMD.md` beside this file shows how to search it with the
indexer every sandbox has.

altitude keeps these files in step between the machines that run the project, so
they are the place to put something the next task needs, not a scratchpad for
what only this task is doing. Write files of your own rather than editing this
README, which altitude refreshes.
