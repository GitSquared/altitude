#!/usr/bin/env python3
"""Makes a sandbox's own listening ports reachable on Altitude's loopback.

A program run in this sandbox listens on the sandbox's loopback, and the browser
Altitude opens for it reaches Altitude's machine instead. This watches the
sandbox's TCP listeners and announces each one over the relay channel Altitude
gave this sandbox; Altitude binds the same number on its own loopback when that
number is free.

For each client that reaches Altitude, Altitude asks for one connection over the
channel; this dials a fresh connection back to the host and joins it to the
sandbox's own service. Nothing here listens on a port, and nothing is reachable
until someone on the host asks.

The channel is host CID 2 on ``ALTITUDE_PORTS_PORT``. A listener is announced with
``W <port>`` and held open for as long as it exists; a client is carried with
``D <port>`` followed by the connection's bytes.
"""

import os
import signal
import socket
import threading
import time

# Host CID is VMADDR_CID_HOST. The port must match PORT_RELAY_GUEST_PORT in
# src/isolation/relay.rs.
HOST_CID = 2
PORT = int(os.environ.get("ALTITUDE_PORTS_PORT", "5001"))

# The terminal's own signals belong to the program the operator is signing in to.
signal.signal(signal.SIGINT, signal.SIG_IGN)
signal.signal(signal.SIGHUP, signal.SIG_IGN)

# How often the sandbox's listeners are read, and how long a port that could not
# be announced waits before trying again. The wait covers a host port that is in
# use: it may be free later, and the sandbox still has the listener.
POLL_SECONDS = 0.25
RETRY_SECONDS = 5.0

# Below this a port is not announced: Binding it needs privileges Altitude's
# machine may not give, and what listens there is the sandbox's own plumbing.
FIRST_FORWARDED_PORT = 1024


def listening_ports():
    """Every TCP port this sandbox is listening on, from the kernel's own tables."""
    ports = set()
    for path in ("/proc/net/tcp", "/proc/net/tcp6"):
        try:
            with open(path, "r") as table:
                next(table, None)
                for line in table:
                    fields = line.split()
                    # 0A is TCP_LISTEN; the local address is `ADDRESS:PORT` in hex.
                    if len(fields) > 3 and fields[3] == "0A":
                        port = fields[1].rpartition(":")[2]
                        try:
                            ports.add(int(port, 16))
                        except ValueError:
                            pass
        except OSError:
            continue
    return ports


def pipe(source, sink):
    """Carry one direction until it ends, then half-close the other."""
    try:
        while True:
            data = source.recv(65536)
            if not data:
                break
            sink.sendall(data)
    except OSError:
        pass
    try:
        sink.shutdown(socket.SHUT_WR)
    except OSError:
        pass


def carry(port):
    """One client: ask Altitude for a connection and join it to the service."""
    relay = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    service = None
    try:
        relay.connect((HOST_CID, PORT))
        relay.sendall(("D %d\n" % port).encode())
        # `localhost` is both families, and a server may have taken the one that
        # is not the first: try every address it resolves to.
        service = socket.create_connection(("localhost", port), timeout=10)
        service.settimeout(None)
        relay.settimeout(None)
        answering = threading.Thread(target=pipe, args=(service, relay), daemon=True)
        answering.start()
        pipe(relay, service)
        answering.join()
    except OSError:
        pass
    finally:
        for connection in (relay, service):
            if connection is not None:
                try:
                    connection.close()
                except OSError:
                    pass


def orders(port, control, retry_at, stop, closing):
    """One listener: one client per byte Altitude sends, until the host hangs up."""
    try:
        while True:
            signal = control.recv(1)
            if signal == b"1":
                threading.Thread(target=carry, args=(port,), daemon=True).start()
            elif signal == b"R":
                # No free number on the host for this port yet. The listener is
                # still here either way, so trying again later is the retry policy.
                retry_at[port] = time.monotonic() + RETRY_SECONDS
                break
            elif signal:
                continue
            else:
                # Altitude let go of the relay: this helper has nothing left to
                # announce to. The listener ending on its own is not that.
                if not closing.is_set():
                    stop.set()
                break
    except OSError:
        pass
    finally:
        try:
            control.close()
        except OSError:
            pass


def main():
    controls = {}
    retry_at = {}
    stop = threading.Event()
    while not stop.is_set():
        now = time.monotonic()
        current = listening_ports()
        for port in list(controls):
            if port not in current or controls[port][0].fileno() < 0:
                control, closing = controls.pop(port)
                closing.set()
                # Shut down rather than only close: the reader thread is parked in
                # recv, and a shutdown is what wakes it.
                try:
                    control.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                control.close()
        for port in current:
            if port < FIRST_FORWARDED_PORT or port in controls:
                continue
            if retry_at.get(port, 0) > now:
                continue
            control = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
            try:
                control.connect((HOST_CID, PORT))
                control.sendall(("W %d\n" % port).encode())
            except OSError:
                control.close()
                retry_at[port] = now + RETRY_SECONDS
                continue
            closing = threading.Event()
            controls[port] = (control, closing)
            threading.Thread(
                target=orders,
                args=(port, control, retry_at, stop, closing),
                daemon=True,
            ).start()
        time.sleep(POLL_SECONDS)


if __name__ == "__main__":
    main()
