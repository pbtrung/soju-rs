#!/usr/bin/env python3
"""Compare Linux RSS/PSS for soju binaries with fresh SQLite databases."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests/compat"))
from ircc import Client  # noqa: E402


def sample(proc, directory, phase, settle):
    time.sleep(settle)
    if proc.poll() is not None:
        raise RuntimeError(f"soju exited; see {directory / 'soju.log'}")
    procdir = Path(f"/proc/{proc.pid}")
    status = (procdir / "status").read_text()
    rollup = (procdir / "smaps_rollup").read_text()
    values = {}
    for key in ("VmRSS", "VmSize", "RssAnon", "RssFile", "RssShmem", "Threads"):
        values[key] = int(re.search(rf"^{key}:\s*(\d+)", status, re.M)[1])
    values["Pss"] = int(re.search(r"^Pss:\s*(\d+)", rollup, re.M)[1])
    (directory / f"{phase}.smaps").write_text((procdir / "smaps").read_text())
    return values


def measure(binary, directory, workers, args):
    config = directory / "config"
    admin = directory / "admin"
    config.write_text(
        f"db sqlite3 {directory}/soju.db\n"
        "hostname soju.test\n"
        f"listen irc+insecure://127.0.0.1:{args.irc_port}\n"
        f"listen ws+insecure://127.0.0.1:{args.ws_port}\n"
        f"listen unix+admin://{admin}\nhttp-origin *\n"
    )
    env = {**os.environ, "SOJU_LOOPS": str(workers), "TOKIO_WORKER_THREADS": str(workers)}
    with (directory / "soju.log").open("w") as log:
        proc = subprocess.Popen(
            [str(binary), "-config", str(config)], env=env,
            stdout=log, stderr=subprocess.STDOUT,
        )
        client = None
        try:
            deadline = time.monotonic() + 15
            while not admin.exists():
                if proc.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(f"soju failed to start; see {directory / 'soju.log'}")
                time.sleep(.05)
            yield "idle", sample(proc, directory, "idle", args.settle)
            subprocess.run(
                [str(binary.parent / "sojuctl"), "-config", str(config),
                 "user", "create", "-username", "alice", "-password", "secret", "-admin"],
                env=env, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=30,
            )
            client = Client(args.irc_port)
            reply = client.register(
                "alice", "alice", sasl=("alice", "secret"),
                caps=["sasl", "batch", "server-time", "message-tags", "soju.im/bouncer-networks"],
            )
            if reply["command"] != "001":
                raise RuntimeError(f"registration failed: {reply}")
            client.roundtrip()
            yield "logged-in", sample(proc, directory, "logged-in", args.settle)
        finally:
            if client is not None:
                client.close()
            if proc.poll() is None:
                proc.terminate()
            try:
                proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
                raise RuntimeError(f"soju shutdown timed out; see {directory / 'soju.log'}")
            if proc.returncode != 0:
                raise RuntimeError(f"soju exited with {proc.returncode}; see {directory / 'soju.log'}")


def positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", action="append", required=True, metavar="NAME=PATH",
                        help="repeat for each soju; matching sojuctl must be alongside it")
    parser.add_argument("--workers", nargs="+", type=positive, default=[4, 12])
    parser.add_argument("--repeat", type=positive, default=1)
    parser.add_argument("--settle", type=float, default=.5)
    parser.add_argument("--irc-port", type=positive, default=18670)
    parser.add_argument("--ws-port", type=positive, default=18671)
    parser.add_argument("--output", type=Path, help="new directory; defaults to a directory in /tmp")
    args = parser.parse_args()
    if args.settle < 0 or args.irc_port > 65535 or args.ws_port > 65535:
        parser.error("invalid settling delay or port")
    if args.irc_port == args.ws_port:
        parser.error("IRC and WebSocket ports must differ")
    binaries = {}
    for item in args.binary:
        name, separator, path = item.partition("=")
        if not separator or not re.fullmatch(r"[A-Za-z0-9_-]+", name) or name in binaries:
            parser.error("binaries need unique NAME=PATH names using letters, digits, _ or -")
        binary = Path(path).resolve()
        if not all(os.access(p, os.X_OK) for p in (binary, binary.parent / "sojuctl")):
            parser.error(f"missing executable soju or sibling sojuctl: {binary}")
        binaries[name] = binary
    output = args.output
    if output is None:
        output = Path(tempfile.mkdtemp(prefix="soju-memory-", dir="/tmp"))
    else:
        output = output.resolve()
        output.mkdir(parents=True, exist_ok=False)
    if re.search(r"\s", str(output)):
        parser.error("output path must not contain whitespace (SCFG paths)")
    metadata = {
        "platform": platform.platform(), "python": platform.python_version(),
        "settle_seconds": args.settle, "units": "KiB except Threads",
        "binaries": {name: {"path": str(path),
                            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                            "version": subprocess.check_output(
                                [str(path), "-version"], text=True, timeout=10).strip()}
                     for name, path in binaries.items()},
    }
    (output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(f"Artifacts: {output}", flush=True)
    results = []
    for repeat in range(1, args.repeat + 1):
        for workers in args.workers:
            for name, binary in binaries.items():
                directory = output / f"{name}-{workers}-{repeat}"
                directory.mkdir()
                for phase, values in measure(binary, directory, workers, args):
                    row = dict(name=name, workers=workers, repeat=repeat, phase=phase, **values)
                    results.append(row)
                    (output / "measurements.json").write_text(json.dumps(results, indent=2) + "\n")
                    print(json.dumps(row), flush=True)


if __name__ == "__main__":
    main()
