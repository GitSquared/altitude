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

Altitude refreshes this file during provisioning.
