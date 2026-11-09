#!/usr/bin/env python3
"""Drives the real Altitude TUI from a script, so an agent that has no terminal of
its own can exercise it.

tmux does this well and this does not need it: Altitude is started on a pty, keys
are written to the master, and the bytes it draws are fed to a small terminal
parser that keeps the visible screen. That screen is what a script asserts on.

Two ways in:

    # A journey written as a file, one step per line (see the docstring of
    # `run_steps` for the steps), printing the screen as it goes.
    scripts/tui.py --root /tmp/altitude-tui --script journey.steps

    # The same thing from Python, for a journey that needs to branch.
    from tui import Tui
    with Tui(root="/tmp/altitude-tui") as tui:
        tui.wait_for("Tasks")
        tui.key("tab")
        print(tui.screen())

The home is built, not copied: `--root` gets an empty config, data and cache
directory, and the binary creates and migrates its database on first boot. Pass
`--config` to point at a `config.toml` to copy in instead.
"""

import argparse
import fcntl
import os
import pty
import re
import shlex
import signal
import struct
import subprocess
import sys
import termios
import threading
import time
import unicodedata

# The keys a script names, and the bytes a terminal would send for them.
KEYS = {
    "enter": b"\r",
    "return": b"\r",
    "tab": b"\t",
    "esc": b"\x1b",
    "escape": b"\x1b",
    "space": b" ",
    "backspace": b"\x7f",
    "delete": b"\x1b[3~",
    "up": b"\x1b[A",
    "down": b"\x1b[B",
    "right": b"\x1b[C",
    "left": b"\x1b[D",
    "home": b"\x1b[H",
    "end": b"\x1b[F",
    "pageup": b"\x1b[5~",
    "pagedown": b"\x1b[6~",
    "ctrl-c": b"\x03",
    "ctrl-d": b"\x04",
    "ctrl-l": b"\x0c",
    "ctrl-s": b"\x13",
}


def width(character):
    if unicodedata.combining(character):
        return 0
    if unicodedata.east_asian_width(character) in ("W", "F"):
        return 2
    return 1


class Screen:
    """Enough of a terminal to keep the grid Altitude draws.

    Ratatui writes cells with cursor positioning and colour, not with newlines, so
    the screen is built by following `CUP`/`CUU`/`CUD`/`CUF`/`CUB`, `ED` and `EL`,
    and ignoring everything that only paints.
    """

    def __init__(self, rows, cols):
        self.rows = rows
        self.cols = cols
        self.reset()

    def reset(self):
        self.grid = [[" "] * self.cols for _ in range(self.rows)]
        self.row = 0
        self.col = 0
        self.saved = (0, 0)
        self.top = 0
        self.bottom = self.rows - 1
        self._state = "normal"
        self._sequence = ""

    def _put(self, character):
        advance = width(character)
        if advance == 0:
            # A combining mark belongs to the cell before it.
            if self.col > 0:
                self.grid[self.row][self.col - 1] += character
            return
        if self.col >= self.cols:
            return
        self.grid[self.row][self.col] = character
        if advance == 2 and self.col + 1 < self.cols:
            self.grid[self.row][self.col + 1] = ""
        self.col += advance

    def _newline(self):
        self.row += 1
        if self.row > self.bottom:
            self._scroll_up(1)
            self.row = self.bottom

    def _scroll_up(self, count):
        for _ in range(count):
            del self.grid[self.top]
            self.grid.insert(self.bottom, [" "] * self.cols)

    def _scroll_down(self, count):
        for _ in range(count):
            del self.grid[self.bottom]
            self.grid.insert(self.top, [" "] * self.cols)

    def _clear_row(self, row, start, end):
        for column in range(max(start, 0), min(end, self.cols)):
            self.grid[row][column] = " "

    def _erase_display(self, mode):
        if mode == 0:
            self._clear_row(self.row, self.col, self.cols)
            for row in range(self.row + 1, self.rows):
                self._clear_row(row, 0, self.cols)
        elif mode == 1:
            self._clear_row(self.row, 0, self.col + 1)
            for row in range(0, self.row):
                self._clear_row(row, 0, self.cols)
        else:
            for row in range(self.rows):
                self._clear_row(row, 0, self.cols)

    def _erase_line(self, mode):
        if mode == 0:
            self._clear_row(self.row, self.col, self.cols)
        elif mode == 1:
            self._clear_row(self.row, 0, self.col + 1)
        else:
            self._clear_row(self.row, 0, self.cols)

    def _apply_csi(self, final, params):
        numbers = []
        for part in params.split(";"):
            numbers.append(int(part) if part.isdigit() else 0)
        first = numbers[0] if numbers and numbers[0] else 1

        def nth(index, default):
            return numbers[index] if len(numbers) > index and numbers[index] else default

        if final in ("H", "f"):
            self.row = min(max(nth(0, 1) - 1, 0), self.rows - 1)
            self.col = min(max(nth(1, 1) - 1, 0), self.cols - 1)
        elif final == "A":
            self.row = max(self.row - first, 0)
        elif final == "B":
            self.row = min(self.row + first, self.rows - 1)
        elif final == "C":
            self.col = min(self.col + first, self.cols - 1)
        elif final == "D":
            self.col = max(self.col - first, 0)
        elif final == "E":
            self.row = min(self.row + first, self.rows - 1)
            self.col = 0
        elif final == "F":
            self.row = max(self.row - first, 0)
            self.col = 0
        elif final == "G":
            self.col = min(max(first - 1, 0), self.cols - 1)
        elif final == "d":
            self.row = min(max(first - 1, 0), self.rows - 1)
        elif final == "J":
            self._erase_display(numbers[0] if numbers else 0)
        elif final == "K":
            self._erase_line(numbers[0] if numbers else 0)
        elif final == "S":
            self._scroll_up(first)
        elif final == "T":
            self._scroll_down(first)
        elif final == "X":
            self._clear_row(self.row, self.col, self.col + first)
        elif final == "r":
            self.top = max(nth(0, 1) - 1, 0)
            self.bottom = min(nth(1, self.rows) - 1, self.rows - 1)
        elif final == "s":
            self.saved = (self.row, self.col)
        elif final == "u":
            self.row, self.col = self.saved

    def feed(self, text):
        for character in text:
            state = self._state
            if state == "normal":
                if character == "\x1b":
                    self._state = "esc"
                elif character == "\n":
                    self._newline()
                elif character == "\r":
                    self.col = 0
                elif character == "\b":
                    self.col = max(self.col - 1, 0)
                elif character == "\t":
                    self.col = min((self.col // 8 + 1) * 8, self.cols - 1)
                elif character >= " " and character != "\x7f":
                    self._put(character)
            elif state == "esc":
                if character == "[":
                    self._sequence = ""
                    self._state = "csi"
                elif character == "]":
                    self._state = "string"
                elif character in "PX^_":
                    self._state = "string"
                elif character == "7":
                    self.saved = (self.row, self.col)
                    self._state = "normal"
                elif character == "8":
                    self.row, self.col = self.saved
                    self._state = "normal"
                elif character in "()*+":
                    self._state = "charset"
                elif character == "c":
                    self.reset()
                else:
                    self._state = "normal"
            elif state == "charset":
                self._state = "normal"
            elif state == "csi":
                if 0x40 <= ord(character) <= 0x7E:
                    self._apply_csi(character, self._sequence.lstrip("?<>="))
                    self._state = "normal"
                else:
                    self._sequence += character
            elif state in ("osc", "string"):
                if character == "\x07":
                    self._state = "normal"
                elif character == "\x1b":
                    self._state = state + "-esc"
            elif state in ("osc-esc", "string-esc"):
                # A string control ends at ST (`ESC \`); anything else was part of
                # the payload and the string goes on.
                self._state = "normal" if character == "\\" else state[:-4]

    def text(self):
        lines = ["".join(row).rstrip() for row in self.grid]
        while lines and not lines[-1]:
            lines.pop()
        return "\n".join(lines)


class Tui:
    def __init__(self, binary, root, rows=40, cols=120, config=None, environment=None):
        self.binary = binary
        self.root = root
        self.rows = rows
        self.cols = cols
        self.config = config
        self.environment = environment or {}
        self.screen = Screen(rows, cols)
        self.raw = bytearray()
        self.master = None
        self.child = None
        self._reader = None
        self._lock = threading.Lock()

    def _home(self):
        home = {
            "ALTITUDE_DATA_DIR": os.path.join(self.root, "data"),
            "ALTITUDE_CACHE_DIR": os.path.join(self.root, "cache"),
            "ALTITUDE_CONFIG_DIR": os.path.join(self.root, "config"),
        }
        for directory in home.values():
            os.makedirs(directory, exist_ok=True)
        if self.config:
            os.makedirs(home["ALTITUDE_CONFIG_DIR"], exist_ok=True)
            with open(self.config, "rb") as source:
                with open(os.path.join(home["ALTITUDE_CONFIG_DIR"], "config.toml"), "wb") as target:
                    target.write(source.read())
        return home

    def start(self):
        environment = dict(os.environ)
        environment.update(self._home())
        environment.update(self.environment)
        environment.setdefault("TERM", "xterm-256color")
        environment["COLUMNS"] = str(self.cols)
        environment["LINES"] = str(self.rows)

        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", self.rows, self.cols, 0, 0))
        self.child = subprocess.Popen(
            [self.binary],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=environment,
            close_fds=True,
            start_new_session=True,
        )
        os.close(slave)
        self.master = master
        self._reader = threading.Thread(target=self._read, daemon=True)
        self._reader.start()
        return self

    def _read(self):
        while True:
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                return
            if not chunk:
                return
            with self._lock:
                self.raw.extend(chunk)
                self.screen.feed(chunk.decode("utf-8", "replace"))

    def send(self, text):
        os.write(self.master, text.encode("utf-8"))
        return self

    def key(self, name):
        for part in name.split("+"):
            part = part.strip()
            if part in KEYS:
                os.write(self.master, KEYS[part])
            elif len(part) == 1:
                os.write(self.master, part.encode("utf-8"))
            else:
                raise KeyError(f"unknown key {part!r}")
        return self

    def text(self):
        with self._lock:
            return self.screen.text()

    def raw_text(self):
        with self._lock:
            return bytes(self.raw).decode("utf-8", "replace")

    def wait_for(self, needle, timeout=20.0, settle=0.15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if needle in self.text():
                time.sleep(settle)
                return self
            if self.child.poll() is not None:
                raise RuntimeError(
                    f"altitude exited with {self.child.returncode} before {needle!r} appeared"
                )
            time.sleep(0.05)
        raise TimeoutError(f"{needle!r} did not appear:\n{self.text()}")

    def wait_for_exit(self, timeout=20.0):
        try:
            return self.child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            raise TimeoutError("altitude did not exit")

    def stop(self, signal_number=signal.SIGTERM):
        if self.child and self.child.poll() is None:
            try:
                os.killpg(self.child.pid, signal_number)
            except ProcessLookupError:
                pass
            try:
                self.child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(self.child.pid, signal.SIGKILL)
                self.child.wait(timeout=5)
        if self.master is not None:
            try:
                os.close(self.master)
            except OSError:
                pass
            self.master = None

    def __enter__(self):
        return self.start()

    def __exit__(self, *_):
        self.stop()


def run_steps(tui, steps, out=sys.stdout):
    """Runs a journey. Each non-blank, non-`#` line is one of:

        send <keys>          keys named as in KEYS, `+`-joined, or text
        send-text <text>     literal text
        wait <text>          wait for text to be on screen (quoted if it has spaces)
        sleep <seconds>
        screen               print the current screen
        raw                  print everything the terminal drew
    """
    for number, line in enumerate(steps, start=1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        command, _, rest = line.partition(" ")
        rest = rest.strip()
        if command in ("send", "send-text", "wait"):
            argument = shlex.split(rest)[0] if rest else ""
        if command == "send":
            tui.key(argument)
        elif command == "send-text":
            tui.send(argument)
        elif command == "wait":
            tui.wait_for(argument)
        elif command == "sleep":
            time.sleep(float(rest or "0.5"))
        elif command == "screen":
            print(f"--- screen after step {number} ---", file=out)
            print(tui.text(), file=out)
        elif command == "raw":
            print(f"--- raw after step {number} ---", file=out)
            print(tui.raw_text(), file=out)
        else:
            raise SystemExit(f"line {number}: unknown step {command!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", default="/tmp/altitude-tui", help="the home to build")
    parser.add_argument("--binary", default=None, help="the altitude binary to run")
    parser.add_argument("--config", default=None, help="a config.toml to copy in")
    parser.add_argument("--rows", type=int, default=40)
    parser.add_argument("--cols", type=int, default=120)
    parser.add_argument("--isolation", default="testbed", help="ALTITUDE_ISOLATION value")
    parser.add_argument("--script", default=None, help="a journey file to run")
    parser.add_argument("--env", action="append", default=[], help="KEY=VALUE, repeatable")
    args = parser.parse_args()

    root = os.path.abspath(args.root)
    binary = args.binary
    if binary is None:
        for candidate in ("target/debug/altitude", "target/release/altitude"):
            if os.path.isfile(candidate):
                binary = os.path.abspath(candidate)
                break
    if binary is None:
        raise SystemExit("no altitude binary found; build it or pass --binary")

    environment = {"ALTITUDE_ISOLATION": args.isolation}
    for entry in args.env:
        key, _, value = entry.partition("=")
        environment[key] = value

    steps = []
    if args.script:
        with open(args.script) as handle:
            steps = handle.readlines()
    else:
        steps = ["wait altitude", "screen"]

    with Tui(binary, root, args.rows, args.cols, args.config, environment) as tui:
        run_steps(tui, steps)


if __name__ == "__main__":
    main()
