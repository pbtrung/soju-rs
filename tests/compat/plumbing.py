"""Tests listeners and transport features of a bouncer: TLS + ALPN, client
certificates with SASL EXTERNAL, PROXY protocol, Prometheus, identd,
SIGHUP reload and upstream TLS with CertFP.

Usage: plumbing.py <rs|go>   (expects env-up.sh to have started the ergo
servers; starts its own bouncer instance on ports 1679x)
"""

import base64
import hashlib
import os
import signal
import socket
import ssl
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(__file__))
from ircc import Client, parse  # noqa: E402

WORK = os.environ.get("COMPAT_WORK", os.path.join(os.path.dirname(os.path.abspath(__file__)), "work"))
# Unix socket paths are limited to ~108 bytes
SOCK = os.environ.get("COMPAT_SOCK_DIR", "/tmp")


S = WORK
KIND = sys.argv[1]
BIN = os.environ.get("RS_BIN", os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../target/debug")) if KIND == "rs" else (os.environ.get("CBIN", os.path.abspath("build/clang-asan")) if KIND == "c" else f"{S}/bin")
ERGO = {"rs": 16667, "go": 16668, "c": 16669}[KIND]
ERGO_TLS = ERGO + 30
D = f"{S}/env/plumb-{KIND}"
CERTS = f"{S}/env/certs"
PORT_TLS, PORT_PLAIN, PORT_IDENT, PORT_PROM = 16790, 16791, 16793, 16794

results = []


def check(name, cond, detail=""):
    results.append((name, bool(cond)))
    print(f"{'ok  ' if cond else 'FAIL'} {name} {detail if not cond else ''}")


class TLSClient(Client):
    def __init__(self, port, cert=None, key=None, alpn=None):
        ctx = ssl.create_default_context(cafile=f"{CERTS}/cert.pem")
        if cert:
            ctx.load_cert_chain(cert, key)
        if alpn:
            ctx.set_alpn_protocols(alpn)
        raw = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.sock = ctx.wrap_socket(raw, server_hostname="localhost")
        self.buf = b""
        self.lines = []
        self.name = "tls"
        self.verbose = False


def start():
    subprocess.run(["rm", "-rf", D, f"{SOCK}/plumb-{KIND}"])
    os.makedirs(D)
    with open(f"{D}/motd", "w") as f:
        f.write("first motd\n")
    with open(f"{D}/config", "w") as f:
        f.write(f"""db sqlite3 {D}/soju.db
hostname soju.test
tls {CERTS}/cert.pem {CERTS}/key.pem
motd {D}/motd
listen ircs://127.0.0.1:{PORT_TLS}
listen irc+insecure://127.0.0.1:{PORT_PLAIN}
listen ident://127.0.0.1:{PORT_IDENT}
listen http+prometheus://localhost:{PORT_PROM}
listen unix+admin://{SOCK}/plumb-{KIND}
accept-proxy-ip localhost
client-cert-auth true
""")
    p = subprocess.Popen([f"{BIN}/soju", "-config", f"{D}/config"], stdout=open(f"{D}/soju.log", "w"), stderr=subprocess.STDOUT)
    for _ in range(50):
        if os.path.exists(f"{SOCK}/plumb-{KIND}"):
            break
        time.sleep(0.1)
    subprocess.run([f"{BIN}/sojuctl", "-config", f"{D}/config", "user", "create", "-username", "alice", "-password", "secret", "-admin"], check=True, stdout=subprocess.DEVNULL)
    return p


def main():
    proc = start()
    try:
        run()
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(5)
        except subprocess.TimeoutExpired:
            proc.kill()
    failed = [n for n, ok in results if not ok]
    print(f"{KIND}: {len(results) - len(failed)}/{len(results)} passed")
    sys.exit(1 if failed else 0)


def run():
    # TLS with ALPN "irc"
    c = TLSClient(PORT_TLS, alpn=["irc"])
    check("tls alpn", c.sock.selected_alpn_protocol() == "irc", c.sock.selected_alpn_protocol())
    r = c.register("alice", "alice", password="secret")
    check("tls login", r["command"] == "001", r)
    motd = [l for l in c.roundtrip() if " 372 " in l]
    check("motd", motd and motd[0].endswith("first motd"), motd)

    # Prometheus
    body = urllib.request.urlopen(f"http://localhost:{PORT_PROM}/metrics", timeout=5).read().decode()
    check("prometheus users", "soju_users_active 1" in body, body[:200])
    check("prometheus downstreams", "soju_downstreams_active 1" in body)

    # Client certificate: not offered as SASL EXTERNAL without a cert
    nc = TLSClient(PORT_TLS)
    nc.send("CAP LS 302")
    ls = nc.expect("CAP")
    check("no EXTERNAL without cert", "sasl=PLAIN" in ls["params"][-1] and "EXTERNAL" not in ls["params"][-1], ls)
    check("client-cert cap", "soju.im/client-cert" in ls["params"][-1])
    nc.close()

    cc = TLSClient(PORT_TLS, cert=f"{CERTS}/client-cert.pem", key=f"{CERTS}/client-key.pem")
    cc.send("CAP LS 302")
    ls = cc.expect("CAP")
    check("EXTERNAL offered with cert", "EXTERNAL" in ls["params"][-1], ls)
    cc.send("PASS secret")
    cc.send("NICK alice")
    cc.send("USER alice 0 * :x")
    cc.send("CAP END")
    cc.expect("001")
    cc.roundtrip()
    cc.send("CLIENTCERT CREATE laptop")
    r = cc.expect("CLIENTCERT")
    check("clientcert create", r["params"] == ["CREATE"], r)
    cc.send("CLIENTCERT LIST")
    lst = [parse(l) for l in cc.roundtrip() if "CLIENTCERT LIST" in l]
    der = open(f"{CERTS}/client-cert.pem").read()
    der = base64.b64decode("".join(der.strip().splitlines()[1:-1]))
    fp = hashlib.sha512(der).hexdigest().upper()
    check("clientcert list", lst and lst[0]["params"][1] == fp and "name=laptop" in lst[0]["params"][2], lst)
    cc.send("PRIVMSG BouncerServ :device-certificate status")
    st = [l for l in cc.roundtrip() if "BouncerServ" in l]
    check("device-certificate status", st and "[127.0.0.1] [laptop]" in st[-1], st)
    cc.close()

    ext = TLSClient(PORT_TLS, cert=f"{CERTS}/client-cert.pem", key=f"{CERTS}/client-key.pem")
    ext.send("CAP LS 302")
    ext.send("CAP REQ :sasl")
    ext.send("NICK whatever")
    ext.send("USER x 0 * :x")
    ext.expect("CAP")
    ext.send("AUTHENTICATE EXTERNAL")
    ext.wait(lambda m, l: m["command"] == "AUTHENTICATE")
    ext.send("AUTHENTICATE +")
    r = ext.wait(lambda m, l: m["command"] in ("903", "904"))
    check("sasl external", r["command"] == "903", r)
    ext.send("CAP END")
    r = ext.wait(lambda m, l: m["command"] in ("001", "ERROR"))
    check("sasl external login", r["command"] == "001" and r["params"][0] == "alice", r)
    ext.close()

    # PROXY protocol v1 from a trusted IP: the downstream hostname comes
    # from the header
    p = Client(PORT_PLAIN)
    p.sock.sendall(b"PROXY TCP4 203.0.113.7 127.0.0.1 40000 16791\r\n")
    p.register("alice", "alice", password="secret")
    p.send("WHO alice")
    who = [parse(l) for l in p.roundtrip() if " 352 " in l]
    check("proxy v1 host", who and who[0]["params"][3] == "203.0.113.7", who)
    p.close()

    # PROXY v2
    p = Client(PORT_PLAIN)
    sig = b"\r\n\r\n\x00\r\nQUIT\n"
    body = bytes([198, 51, 100, 9, 127, 0, 0, 1]) + (41000).to_bytes(2, "big") + (16791).to_bytes(2, "big")
    p.sock.sendall(sig + bytes([0x21, 0x11]) + len(body).to_bytes(2, "big") + body)
    p.register("alice", "alice", password="secret")
    p.send("WHO alice")
    who = [parse(l) for l in p.roundtrip() if " 352 " in l]
    check("proxy v2 host", who and who[0]["params"][3] == "198.51.100.9", who)
    p.close()

    # Upstream TLS with a wrong CertFP pin: the error gives the right one
    c.send(f"PRIVMSG BouncerServ :network create -addr ircs://127.0.0.1:{ERGO_TLS} -name tls -certfp {'00' * 32}")
    c.roundtrip()
    time.sleep(2)
    c.send("PRIVMSG BouncerServ :network status")
    st = [l for l in c.roundtrip() if "BouncerServ" in l and "tls" in l]
    srv_der = open(f"{CERTS}/cert.pem").read()
    srv_der = base64.b64decode("".join(srv_der.strip().splitlines()[1:-1]))
    want = hashlib.sha512(srv_der).hexdigest()
    check("certfp mismatch reports fingerprint", st and want in st[-1], st)
    sha256 = hashlib.sha256(srv_der).hexdigest()
    c.send(f"PRIVMSG BouncerServ :network update tls -certfp {sha256}")
    c.roundtrip()
    ok = False
    for _ in range(30):
        time.sleep(0.5)
        c.send("PRIVMSG BouncerServ :network status")
        st = [l for l in c.roundtrip() if "BouncerServ" in l and "tls" in l]
        if st and "[connected" in st[-1]:
            ok = True
            break
    check("certfp pinned connection", ok, st)

    # identd: look up the bouncer's upstream connection to ergo
    out = subprocess.run(["ss", "-tnp"], capture_output=True, text=True).stdout
    local_port = None
    for line in out.splitlines():
        if f"127.0.0.1:{ERGO_TLS}" in line.split()[4:5] and f"pid={proc_pid()}" in line:
            local_port = line.split()[3].rsplit(":", 1)[1]
    if local_port:
        s = socket.create_connection(("127.0.0.1", PORT_IDENT), timeout=5)
        s.sendall(f"{local_port}, {ERGO_TLS}\r\n".encode())
        reply = s.recv(512).decode()
        check("identd userid", "USERID : UNKNOWN :" in reply, reply)
        s.close()
    else:
        check("identd userid", False, "upstream connection not found")
    s = socket.create_connection(("127.0.0.1", PORT_IDENT), timeout=5)
    s.sendall(b"1, 2\r\n")
    check("identd no-user", "1, 2 : ERROR : NO-USER" in s.recv(512).decode())
    s.close()

    # SIGHUP reloads the MOTD
    with open(f"{D}/motd", "w") as f:
        f.write("second motd\n")
    os.kill(proc_pid(), signal.SIGHUP)
    time.sleep(0.5)
    c2 = TLSClient(PORT_TLS)
    c2.register("alice", "alice", password="secret")
    motd = [l for l in c2.roundtrip() if " 372 " in l]
    check("sighup motd reload", motd and motd[0].endswith("second motd"), motd)
    c2.close()
    c.close()


def proc_pid():
    out = subprocess.run(["pgrep", "-f", f"{BIN}/soju -config {D}/config"], capture_output=True, text=True).stdout
    return int(out.split()[0])


if __name__ == "__main__":
    main()
