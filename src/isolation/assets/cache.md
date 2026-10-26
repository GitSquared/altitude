# Altitude shared cache

Guest: `{{guest_path}}`
Host: `{{host_path}}`

This directory is a cache shared by every task in this project on this machine.
Another task may be using it at the same time: read and write it freely, and do
not clear it. Losing it costs a rebuild, never any work.

{{instructions}}
Altitude refreshes this README during provisioning.
