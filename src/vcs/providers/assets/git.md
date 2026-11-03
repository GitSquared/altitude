Altitude points GIT_CONFIG_GLOBAL at this directory's `config` file. Identity and
signing changes made with `git config --global` apply to all tasks using this profile.

Cloning goes through a shared object cache on the host, so a repository this
machine has already pulled does not come down the network again. The `git` in
your `PATH` is Altitude's, and says so on one line while it copies the cached
objects into the clone.
