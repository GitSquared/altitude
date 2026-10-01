Altitude owns launching and resuming Pi sessions. This profile is shared across
its Pi tasks for settings and authentication. Session files live on each task's
disk and travel with its snapshot; recreating a task without its snapshot loses
that conversation.
Log in with /login inside Pi: credentials are stored in this profile and are
shared by every Pi task. Altitude installs Pi through its environment setup at a
pinned version, so an in-session update would not survive the next environment
refresh.
