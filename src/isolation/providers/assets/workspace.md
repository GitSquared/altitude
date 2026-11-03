# Altitude

Altitude is the user's control plane for delegated coding tasks. It starts your
agent process inside this sandbox and lets the user attach to its terminal.
Detaching the terminal does not stop your work. Quitting Altitude suspends the
sandbox; reopening it restores the task and resumes the agent conversation.

Profile mounts persist separately and are shared across tasks. Their READMEs
explain the host-side handoff. Altitude refreshes these notes during provisioning.
