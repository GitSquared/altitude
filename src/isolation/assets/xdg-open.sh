#!/bin/sh
# Altitude's opener. There is no browser in here, so a web address is handed to
# the terminal Altitude is streaming this sandbox through, and Altitude asks the
# operator on the other end of it before anything opens on their machine.
#
# A real xdg-open returns as soon as it has handed the address over, and so does
# this: nothing in the sandbox ever waits for the answer.
url=
for argument in "$@"; do
	case "$argument" in
		http://* | https://*) url=$(printf '%s' "$argument" | tr -d '\000-\037\177') ;;
	esac
done
[ -n "$url" ] || exit 4
# A program may start its opener with its output detached — the `open` package
# does exactly that, in a session of its own — so the sequence goes to the
# terminal rather than to stdout, which the caller has usually thrown away.
#
# 1. The terminal this process is attached to, when it has one.
{ printf '\033]1337;open;%s\007' "$url" > /dev/tty; } 2>/dev/null && exit 0
# 2. A detached opener has no controlling terminal, but the program that started
#    it still holds the pty on its own stdout. Writing there is writing to the
#    terminal Altitude streams.
parent="/proc/$PPID/fd/1"
if [ -c "$parent" ] && { printf '\033]1337;open;%s\007' "$url" > "$parent"; } 2>/dev/null; then
	exit 0
fi
# 3. No terminal at all: stdout is all there is.
printf '\033]1337;open;%s\007' "$url"
exit 0
