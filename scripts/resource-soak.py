#!/usr/bin/env python3
"""Run two isolated wall-clock soaks and retain reproducible resource samples."""
import argparse
import os
import signal
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, default=180)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not 30 <= args.seconds <= 3600:
        parser.error("--seconds must be between 30 and 3600")
    args.output.mkdir(parents=True, exist_ok=True)
    root = Path(__file__).resolve().parent.parent
    env = dict(os.environ, SOJU_SOAK_SECONDS=str(args.seconds))
    commands = {
        "connections-senders": ["cargo", "test", "--release", "--all-features", "--locked", "--test", "resource_soak", "sustained_connections_and_senders", "--", "--ignored", "--nocapture"],
        "slow-push": ["cargo", "test", "--release", "--all-features", "--locked", "--lib", "user::notifications::soak::sustained_slow_push", "--", "--ignored", "--nocapture"],
    }
    processes = []
    start = time.monotonic()
    try:
        for name, command in commands.items():
            log = (args.output / (name + ".log")).open("w")
            process = subprocess.Popen(command, cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            processes.append((name, process, log))
            print(f"RUN {name}: load={args.seconds}s, cooldown=5s; {log.name}", flush=True)
        while any(process.poll() is None for _, process, _ in processes):
            failed = [(name, process.returncode) for name, process, _ in processes if process.poll() not in (None, 0)]
            if failed:
                raise RuntimeError(f"soak failed: {failed}; inspect retained logs")
            if time.monotonic() - start > args.seconds + 600:
                raise TimeoutError("soak build/load/cooldown deadline exceeded")
            time.sleep(1)
        for name, process, _ in processes:
            if process.returncode:
                raise RuntimeError(f"{name} failed: {process.returncode}")
            print(f"PASS {name}", flush=True)
    finally:
        for _, process, log in processes:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
            log.close()
    print("Samples describe these workloads and allocators, not a whole-process memory guarantee.")


if __name__ == "__main__":
    main()
