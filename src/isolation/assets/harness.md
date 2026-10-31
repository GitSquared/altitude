# Altitude, from inside a task

This machine is one task. Altitude found it, provisioned it, and will dispose of
it. Nothing here is the operator's to type: Altitude manages what is outside the
sandbox, and this file says which parts those are so that advice about them
lands in a signal instead of on the operator's desk.

## What only the operator can change

- **The task's project**: its setup command, shared caches, repository, commit
  identity, and tracker route. They live in Altitude's configuration on the
  operator's machine, not in this sandbox. Editing files under `/` or `~` changes
  this task and nothing else.
- **Credentials and sign-ins.** A login lives in a profile Altitude mounts here;
  you can use it and cannot change it.
- **Remotes.** Which machine runs a project's work is the operator's setting.
- **Accepting the task.** Altitude decides when this capsule is disposed of.

## What Altitude does on its own

- **Builds and refreshes the environment.** The project's setup command is a step
  in a base image, rebuilt when the command changes and refreshed periodically.
  A task starts from that image, so software installed *during* a task is gone
  with it — and asking anyone to "refresh the environment" is asking for a
  settings change.
- **Mounts the shared caches.** Listed in the project brief. They are host
  directories: what you write there outlives this task, and another task may be
  writing at the same time.
- **Follows the operator's settings.** A change to a project on the operator's
  machine is pushed to the machine running this task.
- **Keeps this task's record.** Its history, signals, and capsule are Altitude's,
  and they travel with the task when it is moved between machines.

## What that means in practice

- A tool that must be here permanently is a **project setup change**. Say so,
  with the exact command and why, and let the operator decide. Do not install it
  and assume it will still be here.
- Cache contents are worth recommending (a warm registry, a package cache) and
  never worth assuming.
- Never tell the operator to run something inside this sandbox unless it is about
  their own machine: what you can run here, they cannot reach.

## Being taken over, and handed back

The operator can **take this task over**: they stop the agent here, carry the
working files and this conversation to their own machine, and continue with you
there. When that happens your sandbox is not the one you were in. The files under
the checkout travel and so does this conversation, but the machine does not: a
sandbox is built per machine from the project's setup, so anything you installed
by hand, any shell state, and any process you left running are gone. Shared caches
do not travel either — they belong to the machine, and a cache is a shortcut
rather than knowledge.

While the operator is driving, you are talking to them directly in a terminal
rather than through Altitude's signals, and they are likely to be watching what
you do. A take-over sandbox may open network ports for them; Altitude proxies
those to their machine, so say which port you opened and what it serves rather
than telling them to "open localhost" and hoping.

They can **hand the task back** at any moment, and Altitude resumes running it
without them. Assume neither state is permanent: leave the work readable, say
what is unfinished, and never assume the machine you are on now is where you will
finish.

Altitude refreshes this file during provisioning.
