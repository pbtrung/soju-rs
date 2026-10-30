"""Exercise the actual programs: admin commands, IRC, reload and shutdown."""

import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time


BIN = Path(sys.argv[1]).resolve()


def wait_for(predicate, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.01)
    raise AssertionError("timed out waiting for server")


def connect(path):
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(5)
    sock.connect(str(path))
    return sock


def read_until(sock, marker):
    data = b""
    while marker not in data:
        block = sock.recv(65536)
        assert block, ("unexpected EOF", data)
        data += block
    return data


def ctl(config, *words, success=True):
    result = subprocess.run(
        [str(BIN / "sojuctl"), "-config", str(config), *words],
        capture_output=True, text=True, timeout=20,
    )
    assert (result.returncode == 0) == success, (result.stdout, result.stderr)
    assert "Sanitizer" not in result.stderr, result.stderr
    return result.stdout + result.stderr


def scenario(exit_signal):
    with tempfile.TemporaryDirectory(prefix="soju-process-") as directory:
        root = Path(directory)
        config, admin, irc, http = (root / name for name in ("config", "admin", "irc", "http"))
        motd = root / "motd"
        motd.write_text("first motd\n")
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            ident_port = probe.getsockname()[1]
        original = (
            f"db sqlite3 {root / 'soju.db'}\n"
            "hostname before.example\n"
            f"motd {motd}\n"
            f"listen unix+admin://{admin}\n"
            f"listen unix://{irc}\n"
            f"listen http+unix://{http}\n"
            f"listen ident://127.0.0.1:{ident_port}\n"
            "message-expiry 1d\n"
        )
        config.write_text(original)
        with (root / "server.log").open("w+") as log:
            proc = subprocess.Popen([str(BIN / "soju"), "-config", str(config)], stderr=log)
            peers = []
            try:
                wait_for(lambda: admin.exists() or proc.poll() is not None)
                assert proc.poll() is None, "server exited during startup"
                assert "created user" in ctl(config, "user", "create", "-username", "alice",
                    "-password", 'a "b"\\', "-admin")
                assert "alice" in ctl(config, "user", "status")
                ctl(config, "invalid-command", success=False)
                # Quoted passwords survive argv, the admin protocol and storage.
                peer = connect(irc)
                peers.append(peer)
                peer.sendall(b'PASS :a "b"\\\r\nNICK alice\r\nUSER alice 0 * :Alice\r\n')
                welcome = read_until(peer, b" 376 ")
                assert b"before.example" in welcome and b"first motd" in welcome, welcome
                peer.sendall(b"PING alive\r\n")
                assert b"alive" in read_until(peer, b"PONG")
                # Publish a fully loaded snapshot, including the new MOTD.
                motd.write_text("second motd\n")
                config.write_text(original.replace("before.example", "after.example"))
                proc.send_signal(signal.SIGHUP)
                def reloaded():
                    log.flush()
                    return "configuration reloaded" in (root / "server.log").read_text()
                wait_for(reloaded)
                ctl(config, "server", "status")
                second = connect(irc)
                peers.append(second)
                second.sendall(b'PASS :a "b"\\\r\nNICK alice\r\nUSER alice 0 * :Alice\r\n')
                welcome = read_until(second, b" 376 ")
                assert b"after.example" in welcome and b"second motd" in welcome, welcome
                # Failed reload keeps serving with the previous snapshot.
                config.write_text("not-a-directive true\n")
                proc.send_signal(signal.SIGHUP)
                wait_for(lambda: "failed to reload" in (root / "server.log").read_text())
                peer.sendall(b"PING still-alive\r\n")
                assert b"still-alive" in read_until(peer, b"PONG")
                # Shutdown must close idle admin and incomplete HTTP sessions too.
                peers.append(connect(admin))
                idle_ident = socket.create_connection(("127.0.0.1", ident_port), timeout=5)
                peers.append(idle_ident)
                idle_ident.sendall(b"1234,")
                pending_http = connect(http)
                peers.append(pending_http)
                pending_http.sendall(b"GET / HTTP/1.1\r\nHost:")
                proc.send_signal(exit_signal)
                assert proc.wait(timeout=10) == 0
                try:
                    assert pending_http.recv(1) == b""
                except ConnectionResetError:
                    pass  # An unread request may be reset during shutdown.
            finally:
                for peer in peers:
                    peer.close()
                if proc.poll() is None:
                    proc.kill()
                    proc.wait()
                log.flush()
                output = (root / "server.log").read_text()
                if proc.returncode != 0:
                    print(output, file=sys.stderr)
                assert "Sanitizer" not in output and "objects left" not in output, output
        # Startup failures must drain any listeners already opened.
        config.write_text(original + "listen gopher://localhost\n")
        failed = subprocess.run([str(BIN / "soju"), "-config", str(config)],
            capture_output=True, text=True, timeout=15)
        assert failed.returncode == 1, failed.stderr
        assert "Sanitizer" not in failed.stderr and "live object" not in failed.stderr, failed.stderr


scenario(signal.SIGTERM)
scenario(signal.SIGINT)
print("process lifecycle passed")
