#!/bin/sh
# Altitude's git: clones through this machine's shared object cache.
#
# A capsule is disposable, so the repository cloned into it is too, and a machine
# running task after task over the same few repositories was pulling each one
# from the network again every time. This keeps one bare mirror per repository in
# the cache the host mounts, lets the clone borrow the objects the mirror already
# holds, and then copies them into the clone so it stops borrowing.
#
# The copy is the point. A clone that borrowed and kept borrowing would be a view
# of the cache rather than a repository, and a cache is something the operating
# system may reclaim and the operator may delete. Neither should leave a task
# unable to read its own history.
#
# Only `clone` is touched. Everything that can go wrong here — no cache mounted,
# a mirror that cannot be reached or read, a command line this cannot parse — is
# arranged to cost a normal clone rather than a failed one.

set -u

# Named rather than looked up: looking `git` up on the path would find this
# script, and a script that execs itself is not a failure anyone can read.
real=/usr/bin/git

case "${1:-}" in
clone) ;;
*) exec "$real" "$@" ;;
esac

cache=${ALTITUDE_GIT_CACHE:-}
if [ -z "$cache" ] || [ ! -d "$cache" ]; then
    exec "$real" "$@"
fi

# Two tasks on one machine cloning the same repository at the same time is the
# normal case here, not the rare one, so an updater without a lock is not an
# updater. Without one, this leaves the cache alone and clones as git always did.
command -v flock >/dev/null 2>&1 || exec "$real" "$@"

# Bring the mirror up to date, or make it. The caller holds the lock.
update_mirror() {
    if [ -d "$mirror" ] && "$real" -C "$mirror" rev-parse --is-bare-repository >/dev/null 2>&1; then
        # An update that fails is not a clone that fails: what the mirror already
        # holds is still worth borrowing.
        "$real" -C "$mirror" fetch --prune --quiet 2>/dev/null || :
    else
        # Absent, or left half-made by an interrupted clone: either way there is
        # nothing here to update, and a clone needs the path to itself.
        rm -rf "$mirror" 2>/dev/null
        "$real" clone --bare --quiet "$repo" "$mirror" 2>/dev/null || :
        # Branches and tags rather than `--mirror`, which asks for `refs/*` and is
        # answered with every pull request the remote has ever served: 8,675 of
        # ripgrep's 22,737 objects, which no task looks at and every clone that
        # copies these packs carries.
        "$real" -C "$mirror" config remote.origin.fetch '+refs/heads/*:refs/heads/*' 2>/dev/null || :
        "$real" -C "$mirror" config --add remote.origin.fetch '+refs/tags/*:refs/tags/*' 2>/dev/null || :
    fi
}

# The repository: the first argument that is neither an option nor the value of
# one. Guessing wrong is survivable, because the option this ends up passing only
# borrows from a mirror if there is one there, so the worst an unreadable command
# line costs is an uncached clone.
repo=
for arg in "$@"; do
    case "$arg" in
    clone | -*) continue ;;
    *://* | *@*:*) repo=$arg && break ;;
    esac
done
[ -n "$repo" ] || exec "$real" "$@"

# Named after the repository rather than after the URL that reached it, so the
# same repository cloned over ssh and over https is one mirror, and so a mirror
# is still readable in a directory listing.
slug=$(printf '%s' "$repo" |
    sed -e 's#^[a-zA-Z+]*://##' -e 's#^[^@/]*@##' -e 's#:#/#g' -e 's#/*$##' \
        -e 's#\.git$##' -e 's#[^A-Za-z0-9._/-]#_#g')
[ -n "$slug" ] || exec "$real" "$@"
mirror="$cache/$slug.git"
mkdir -p "${mirror%/*}" 2>/dev/null || exec "$real" "$@"

# Where the clone is going, so that what it borrowed can be made its own. git is
# told nothing about this: it is worked out the way git works it out and then
# checked against what git actually did, so a command line this misreads costs
# the copy rather than the clone.
directory=
previous=
for arg in "$@"; do
    if [ "$previous" = "$repo" ]; then
        case "$arg" in
        -*) ;;
        *) directory=$arg ;;
        esac
        break
    fi
    previous=$arg
done
[ -n "$directory" ] || directory=${slug##*/}

# Held on a descriptor rather than run around a command, because whether to wait
# depends on what is already here, and what is here can change while waiting.
# BusyBox's flock has no timeout option, so waiting is something this script
# decides rather than something it asks for.
#
# Touched first and opened second, rather than redirecting the open itself: an
# `exec` with no command applies its redirections to this shell for good, so a
# stderr silenced there would silence the clone that comes after it.
if ! touch "$mirror.lock" 2>/dev/null; then
    exec "$real" "$@"
fi
exec 9>"$mirror.lock"

if [ -d "$mirror" ]; then
    # A mirror that is here is usable as it stands, so an update is not worth
    # waiting behind another task for: being a little stale costs the difference
    # between what this machine has and what the remote does.
    flock -n 9 && update_mirror
else
    # The first task over a repository pulls it for all of them, so the tasks
    # behind it wait for that pull instead of making it again themselves.
    flock 9
    update_mirror
fi

# The lock guards the mirror, not the clone that borrows from it.
exec 9>&-

# A mirror that could not be made is a clone that runs as it always did.
if [ ! -d "$mirror" ]; then
    exec "$real" "$@"
fi

# The cache is swept by least recent use, and this is a use. A mirror that could
# not be updated is still worth keeping: a task did clone through it. Only a
# directory is touched, because a file here would be read as a gitfile by the
# option below and fail the clone this is supposed to be speeding up.
touch "$mirror" 2>/dev/null

shift
"$real" clone --reference-if-able "$mirror" "$@"
status=$?
if [ "$status" -ne 0 ]; then
    exit "$status"
fi

# What the clone borrowed is now this repository's, and the borrowing stops.
# Copied rather than repacked, because a pack is already the compressed form of
# exactly these objects, and dropped only once the copy is really there: a
# failure here leaves a clone that still works, reading the cache it borrowed
# from, rather than one that cannot read its own history.
#
# Contents copied and permissions not carried over: the mirror's files belong to
# the host, and preserving their modes is a thing this filesystem refuses.
alternates="$directory/.git/objects/info/alternates"
if [ -f "$alternates" ] && [ "$(cat "$alternates" 2>/dev/null)" = "$mirror/objects" ]; then
    echo "altitude: copying cached objects into $directory" >&2
    if cp -R "$mirror"/objects/pack/. "$directory/.git/objects/pack/" 2>/dev/null; then
        rm -f "$alternates"
    fi
fi

# The clone succeeded, which is all this had to preserve.
exit 0
