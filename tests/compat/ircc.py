"""Tiny synchronous IRC client for scripted tests."""

import base64
import re
import socket
import time


def parse(line):
    tags = {}
    if line.startswith("@"):
        t, line = line[1:].split(" ", 1)
        for kv in t.split(";"):
            k, _, v = kv.partition("=")
            tags[k] = v
    prefix = ""
    if line.startswith(":"):
        prefix, line = line[1:].split(" ", 1)
    if " :" in line:
        mid, trailing = line.split(" :", 1)
        params = mid.split() + [trailing]
    else:
        params = line.split()
    return {"tags": tags, "prefix": prefix, "command": params[0].upper(), "params": params[1:]}


class Client:
    def __init__(self, port, host="127.0.0.1", name="c"):
        self.sock = socket.create_connection((host, port), timeout=10)
        self.buf = b""
        self.lines = []
        self.name = name
        self.verbose = False

    def send(self, line):
        if self.verbose:
            print(f"{self.name} >> {line}")
        self.sock.sendall(line.encode() + b"\r\n")

    def _readline(self, timeout):
        deadline = time.time() + timeout
        while b"\n" not in self.buf:
            remaining = deadline - time.time()
            if remaining <= 0:
                return None
            self.sock.settimeout(remaining)
            try:
                data = self.sock.recv(65536)
            except socket.timeout:
                return None
            if not data:
                raise EOFError(f"{self.name}: connection closed")
            self.buf += data
        line, self.buf = self.buf.split(b"\n", 1)
        line = line.rstrip(b"\r").decode(errors="replace")
        if self.verbose:
            print(f"{self.name} << {line}")
        if line.startswith("PING ") or " PING " in line.split(" :")[0] and not line.startswith("@"):
            m = parse(line)
            if m["command"] == "PING" and not m["params"][0].startswith("soju-msgid-"):
                self.send("PONG :" + m["params"][-1])
        self.lines.append(line)
        return line

    def wait(self, pred, timeout=5, desc=None):
        """Reads until pred(parsed msg, raw line) is true. Returns the message."""
        deadline = time.time() + timeout
        while True:
            remaining = deadline - time.time()
            line = self._readline(max(remaining, 0)) if remaining > 0 else None
            if line is None:
                raise TimeoutError(f"{self.name}: timed out waiting for {desc or pred}")
            m = parse(line)
            if pred(m, line):
                return m

    def expect(self, command, timeout=5):
        return self.wait(lambda m, l: m["command"] == command, timeout, desc=command)

    def drain(self, timeout=0.5):
        """Reads all lines until quiet for `timeout`."""
        out = []
        while True:
            try:
                line = self._readline(timeout)
            except EOFError:
                return out
            if line is None:
                return out
            out.append(line)

    def roundtrip(self, timeout=5):
        """Sends PING and returns all lines received until the PONG."""
        tok = f"rt{time.time_ns()}"
        self.send(f"PING {tok}")
        out = []
        while True:
            line = self._readline(timeout)
            if line is None:
                raise TimeoutError(f"{self.name}: no PONG")
            m = parse(line)
            if m["command"] == "PONG" and m["params"][-1] == tok:
                return out
            out.append(line)

    def register(self, nick, user, password=None, caps=None, sasl=None):
        """Registers. caps: list of caps to request (None: no CAP). sasl: (user, pass)."""
        if caps is not None:
            self.send("CAP LS 302")
        if password:
            self.send(f"PASS {password}")
        self.send(f"NICK {nick}")
        self.send(f"USER {user} 0 * :{nick}")
        if caps is not None:
            ls = self.expect("CAP")
            offered = {c.split("=")[0] for c in ls["params"][-1].split()}
            caps = [c for c in caps if c in offered]
            if caps:
                self.send("CAP REQ :" + " ".join(caps))
                ack = self.wait(lambda m, l: m["command"] == "CAP" and m["params"][1] in ("ACK", "NAK"))
                if ack["params"][1] != "ACK":
                    raise RuntimeError(f"CAP NAK: {ack}")
            if sasl:
                self.send("AUTHENTICATE PLAIN")
                self.wait(lambda m, l: m["command"] == "AUTHENTICATE")
                creds = base64.b64encode(f"\0{sasl[0]}\0{sasl[1]}".encode()).decode()
                self.send(f"AUTHENTICATE {creds}")
                r = self.wait(lambda m, l: m["command"] in ("903", "904", "905", "906"))
                if r["command"] != "903":
                    raise RuntimeError(f"SASL failed: {r}")
            self.send("CAP END")
        return self.wait(lambda m, l: m["command"] in ("001", "464", "ERROR", "FAIL", "432", "433"))

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def norm(line, rules=()):
    """Normalizes volatile parts of a line for comparisons."""
    line = re.sub(r"time=\d{4}-\d\d-\d\dT[\d:.]+Z", "time=T", line)
    line = re.sub(r"timestamp=\d{4}-\d\d-\d\dT[\d:.]+Z", "timestamp=T", line)
    line = re.sub(r"msgid=[^; ]+", "msgid=M", line)
    for a, b in rules:
        line = re.sub(a, b, line)
    return line
