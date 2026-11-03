#!/usr/bin/env python3
"""Carries MCP between a harness's stdio and Altitude, over virtio-vsock.

Altitude listens on a host Unix socket that this sandbox reaches as host CID 2 on
``ALTITUDE_MCP_PORT``. The harness launches this file as an MCP stdio server; it
is a byte pump, so the MCP framing belongs to Altitude and the harness alone.

The connection stays open for the session. Altitude owns the other end and
decides what the tools are.
"""

import os
import socket
import sys
import threading

# Host CID is VMADDR_CID_HOST. The port must match AGENT_CHANNEL_PORT in
# src/agent/mcp.rs; the environment overrides it for a sandbox that was given a
# different route.
HOST_CID = 2
PORT = int(os.environ.get("ALTITUDE_MCP_PORT", "5000"))


def to_host(connection):
    """Everything the harness writes, until its stdin closes."""
    try:
        while True:
            data = os.read(0, 65536)
            if not data:
                break
            connection.sendall(data)
    except OSError:
        pass
    finally:
        # Half-close rather than close: the host may still have an answer in
        # flight, and closing outright would drop it.
        try:
            connection.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def to_stdout(connection):
    """Everything Altitude answers, until the connection closes."""
    while True:
        try:
            data = connection.recv(65536)
        except OSError:
            break
        if not data:
            break
        os.write(1, data)


def main():
    connection = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    connection.connect((HOST_CID, PORT))
    threading.Thread(target=to_host, args=(connection,), daemon=True).start()
    to_stdout(connection)
    connection.close()


if __name__ == "__main__":
    try:
        main()
    except OSError as error:
        # The harness treats a server that cannot start as a failed server and
        # carries on, so a missing channel must not look like a crash.
        print(f"altitude bridge could not reach Altitude: {error}", file=sys.stderr)
        sys.exit(1)
