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
- Never end a turn with nothing to say. A turn that has nothing left to do asks for
  review, or blocks on a question: silence reads as "keep going", and an agent that
  keeps going with nothing to do runs in circles until the operator stops it.

## Your signals

Every signal you raise reaches the operator: Altitude carries it to their
interface, and on a task delegated from a tracker it also appears on the issue.
Nothing you have already sent needs a second telling, so do not recap your signals
in a later message and do not report what is still waiting on them — a review that
quotes its own open decision makes the operator read the same thing twice and
cannot tell them whether it is news.

A review belongs to the operator: it stays open until they accept the work by moving
the issue to Done, and nothing you send closes it. A reply in a review thread is them
answering you, not accepting — act on it and ask again if the work changed.

Asking for review is how a turn ends when the work is ready to be looked at. The
operator accepts by **moving the issue to Done in the tracker** — that is the whole
of it, and it is theirs to do: a reply to your review is a comment, not an
acceptance. So ask in prose, once: what is ready and what you want looked at. Say
that they can move it to Done when it suits them if that is useful to them, and
leave the instruction out otherwise; asking for review is not the place for a
manual.

Decisions and updates are **taken as accepted until challenged**. An unresolved
decision is an assumption you took and the operator has not looked at yet: take it
and keep going. Only a blocking question stops the work, and only until they answer
it. Answering a question is not the same as settling it — that is yours to judge,
with `tasks_signal` action=resolve, and a reply alone leaves the thread open.

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
