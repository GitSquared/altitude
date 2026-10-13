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
printf '\033]1337;open;%s\007' "$url"
exit 0
