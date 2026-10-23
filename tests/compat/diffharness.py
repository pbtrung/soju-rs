"""Differential tests: runs the same scenarios against Go soju and soju-rs
and compares normalized transcripts."""

import base64
import difflib
import os
import re
import signal
import sys
import time
import traceback

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ircc import Client, parse  # noqa: E402

WORK = os.environ.get("COMPAT_WORK", os.path.join(os.path.dirname(os.path.abspath(__file__)), "work"))
# Unix socket paths are limited to ~108 bytes
SOCK = os.environ.get("COMPAT_SOCK_DIR", "/tmp")


ENVS = {
    "go": {"soju": 16680, "ws": 16681, "ergo": 16668},
    "rs": {"soju": 16670, "ws": 16671, "ergo": 16667},
}

ALL_CAPS = [
    "sasl", "batch", "server-time", "message-tags", "echo-message", "labeled-response",
    "soju.im/bouncer-networks", "soju.im/bouncer-networks-notify", "draft/chathistory",
    "draft/read-marker", "multi-prefix", "extended-join", "away-notify", "account-notify",
    "account-tag", "chghost", "setname", "invite-notify", "cap-notify", "draft/extended-isupport",
    "draft/metadata-2", "soju.im/search",
    "soju.im/webpush", "draft/pre-away",
]


def sort_tags(line):
    if not line.startswith("@"):
        return line
    tags, rest = line[1:].split(" ", 1)
    return "@" + ";".join(sorted(tags.split(";"))) + " " + rest


def normalize(line):
    line = re.sub(r"time=\d{4}-\d\d-\d\dT[\d:.]+Z", "time=T", line)
    line = re.sub(r"timestamp=\d{4}-\d\d-\d\dT[\d:.]+Z", "timestamp=T", line)
    line = re.sub(r"\b20\d\d-\d\d-\d\dT[\d:.]+Z", "ISOTIME", line)
    line = re.sub(r"msgid=[^; ]+", "msgid=M", line)
    line = re.sub(r"VAPID=[A-Za-z0-9_-]+", "VAPID=K", line)
    line = re.sub(r"[a-z0-9]{13}\.irc\b", "CLOAK.irc", line)
    line = re.sub(r"\b1666[78]\b", "ERGOPORT", line)
    line = re.sub(r"\b1667[01]\b|\b1668[01]\b", "SOJUPORT", line)
    line = re.sub(r"127\.0\.0\.1:\d{4,5}\b", "127.0.0.1:P", line)
    line = re.sub(r"\b1[789]\d{8}\b", "TS", line)
    line = re.sub(r"rt\d{15,}", "RT", line)
    line = re.sub(r"fingerprint: [0-9a-f]+", "fingerprint: FP", line)
    # TLS libraries word errors differently
    line = re.sub(r"tls:(\\s|:? )[^;]*", "tls: ERR", line)
    line = sort_tags(line)
    m = parse(line)
    cmd, params = m["command"], m["params"]
    # Go iterates maps in random order: sort unordered lists
    if cmd == "CAP" and len(params) >= 3 and params[1] in ("LS", "LIST", "NEW", "DEL", "ACK", "NAK"):
        head = line[: line.rindex(params[-1])]
        line = head + " ".join(sorted(params[-1].split()))
    if cmd == "BOUNCER" and len(params) >= 3 and params[0] == "NETWORK":
        head = line[: line.rindex(params[-1])]
        line = head + ";".join(sorted(params[-1].split(";")))
    if cmd == "353":
        head = line[: line.rindex(params[-1])]
        line = head + " ".join(sorted(params[-1].split()))
    if cmd == "CLIENTCERT" and len(params) >= 3:
        head = line[: line.rindex(params[-1])]
        line = head + ";".join(sorted(params[-1].split(";")))
    return line


class Env:
    def __init__(self, name):
        self.name = name
        self.ports = ENVS[name]
        self.transcripts = {}
        self.clients = []

    def client(self, name, upstream=False):
        port = self.ports["ergo"] if upstream else self.ports["soju"]
        c = Client(port, name=f"{self.name}:{name}")
        c.label = name
        if not upstream:
            self.transcripts.setdefault(name, [])
        c.record = not upstream
        self.clients.append(c)
        return c

    def settle(self, delay=0.3):
        time.sleep(delay)
        for c in self.clients:
            try:
                c.drain(0.15)
            except EOFError:
                pass

    def finish(self):
        self.settle()
        for c in self.clients:
            if c.record:
                self.transcripts[c.label].extend(c.lines)
            c.close()


def reset_env():
    import subprocess
    here = __file__.rsplit("/", 1)[0]
    subprocess.run(["bash", f"{here}/env-up.sh"], check=True, stdout=subprocess.DEVNULL)


def run_scenario(fn):
    reset_env()
    results = {}
    errors = {}
    for name in ("go", "rs"):
        env = Env(name)
        try:
            fn(env)
        except Exception:
            errors[name] = traceback.format_exc()
        try:
            env.finish()
        except Exception:
            pass
        results[name] = env.transcripts
    return results, errors


def block_sort(lines):
    """Sorts runs of consecutive lines sharing the same command (and batch),
    since Go iterates maps in random order."""
    out, block, key = [], [], None
    for l in lines:
        m = parse(l)
        k = (m["command"], m["tags"].get("batch"), m["prefix"].split("!")[0])
        if k != key and block:
            out.extend(sorted(block))
            block = []
        key = k
        block.append(l)
    out.extend(sorted(block))
    return out


def compare(name, results, errors, show=True):
    ok = True
    for k, v in errors.items():
        print(f"  [{k}] scenario error:\n" + "".join("    " + l + "\n" for l in v.splitlines()[-6:]))
        ok = False
    go, rs = results.get("go", {}), results.get("rs", {})
    for client in sorted(set(go) | set(rs)):
        a = block_sort([normalize(l) for l in go.get(client, [])])
        b = block_sort([normalize(l) for l in rs.get(client, [])])
        if a == b:
            continue
        if sorted(a) == sorted(b):
            print(f"  [{client}] order-only difference (accepted: Go iterates maps randomly)")
            continue
        ok = False
        if show:
            diff = difflib.unified_diff(a, b, "go", "rs", lineterm="", n=2)
            for l in list(diff)[:200]:
                print("    " + l)
    print(f"{'PASS' if ok else 'FAIL'} {name}")
    return ok


SCENARIOS = []


def scenario(fn):
    SCENARIOS.append(fn)
    return fn


def login(env, name="c", caps=ALL_CAPS, user="alice", password="secret", sasl=True, nick=None):
    c = env.client(name)
    nick = nick or user.split("/")[0].split("@")[0]
    if sasl:
        c.register(nick, nick, caps=caps, sasl=(user, password))
    else:
        c.register(nick, user, password=password, caps=caps)
    c.roundtrip()
    return c


def add_network(env, c, name="ergo"):
    c.send(f"BOUNCER ADDNETWORK :name={name};host=127.0.0.1;port={env.ports['ergo']};tls=0")
    m = c.wait(lambda m, l: m["command"] == "BOUNCER" and m["params"][0] == "ADDNETWORK")
    netid = m["params"][1]
    c.wait(lambda m, l: m["command"] == "BOUNCER" and m["params"][1] == netid and "state=connected" in l, timeout=10)
    return netid


def actor(env, nick, channels=()):
    a = env.client(nick, upstream=True)
    a.register(nick, nick)
    for ch in channels:
        a.send(f"JOIN {ch}")
        a.wait(lambda m, l, ch=ch: m["command"] == "366" and m["params"][1] == ch)
    return a


# ---------------------------------------------------------------- scenarios


@scenario
def registration_bouncer(env):
    c = login(env)
    c.send("BOUNCER LISTNETWORKS")
    c.roundtrip()
    c.send("WHO alice")
    c.send("WHO alice %tnuhraf,42")
    c.send("WHO BouncerServ")
    c.send("WHO nobody")
    c.send("WHOIS alice")
    c.send("WHOIS BouncerServ")
    c.send("INFO")
    c.send("MODE alice")
    c.send("JOIN #foo")
    c.send("PRIVMSG alice :hi me")
    c.send("FOOBAR x")
    c.send("CAP LIST")
    c.send("ISUPPORT")
    c.send("CHATHISTORY TARGETS timestamp=2020-01-01T00:00:00.000Z timestamp=2030-01-01T00:00:00.000Z 10")
    c.send("MARKREAD #x")
    c.send("BOUNCER BIND 1")
    c.send("BOUNCER DELNETWORK 999")
    c.send("BOUNCER CHANGENETWORK abc :name=x")
    c.send("BOUNCER WHAT")
    c.send("PING :a b")
    c.send("PING x other.server")
    c.send("USER a b c d")
    c.send("METADATA * LIST")
    c.send("METADATA * SUB soju.im/pinned bogus")
    c.send("METADATA * SUBS")
    c.roundtrip()


@scenario
def registration_errors(env):
    c = env.client("nopass")
    c.send("NICK x")
    c.send("USER x 0 * :x")
    c.wait(lambda m, l: m["command"] in ("464", "ERROR"))
    c2 = env.client("badpass")
    c2.send("PASS wrong")
    c2.send("NICK alice")
    c2.send("USER alice 0 * :x")
    c2.wait(lambda m, l: m["command"] in ("464", "ERROR"))
    c3 = env.client("saslrequired")
    c3.send("CAP LS 302")
    c3.send("CAP REQ sasl")
    c3.send("NICK x")
    c3.send("USER x 0 * :x")
    c3.send("CAP END")
    c3.wait(lambda m, l: m["command"] in ("FAIL", "464"))
    c4 = env.client("saslfail")
    c4.send("CAP LS 302")
    c4.send("CAP REQ :sasl unknown-cap")
    c4.send("CAP REQ :sasl")
    c4.send("AUTHENTICATE PLAIN")
    c4.send("AUTHENTICATE " + "AGFsaWNlAHdyb25n")
    c4.send("AUTHENTICATE FOO")
    c4.send("AUTHENTICATE *")
    c4.send("PRIVMSG x :y")
    c4.send("CAP BOGUS")
    c4.send("CAP REQ")
    c4.send("PING x")
    c4.drain(0.5)
    c5 = env.client("passlogin")
    c5.send("PASS secret")
    c5.send("NICK alice")
    c5.send("USER alice/nonexistent-net-name 0 * :x")
    c5.wait(lambda m, l: m["command"] in ("001", "464", "ERROR"), timeout=20)
    c6 = env.client("multi")
    c6.send("PASS secret")
    c6.send("NICK alice")
    c6.send("USER alice/* 0 * :x")
    c6.wait(lambda m, l: m["command"] in ("001", "464", "ERROR"))
    c7 = env.client("clientmismatch")
    c7.send("CAP LS 302")
    c7.send("CAP REQ sasl")
    c7.send("AUTHENTICATE PLAIN")
    c7.send("AUTHENTICATE AGFsaWNlQHBob25lAHNlY3JldA==")
    c7.send("NICK alice")
    c7.send("USER alice@laptop 0 * :x")
    c7.send("CAP END")
    c7.wait(lambda m, l: m["command"] in ("001", "432", "ERROR"))


@scenario
def network_lifecycle(env):
    c = login(env)
    netid = add_network(env, c)
    c.send(f"BOUNCER CHANGENETWORK {netid} :realname=Alice Example")
    c.wait(lambda m, l: m["command"] == "BOUNCER" and m["params"][0] == "CHANGENETWORK")
    c.wait(lambda m, l: m["command"] == "BOUNCER" and "state=connected" in l, timeout=10)
    c.send("BOUNCER ADDNETWORK :name=ergo;host=127.0.0.1")
    c.send("BOUNCER ADDNETWORK :name=-bad;host=x.example")
    c.send("BOUNCER ADDNETWORK :port=1")
    c.send("BOUNCER ADDNETWORK :name=x;host=x.example;bogus=1")
    c.send(f"BOUNCER CHANGENETWORK {netid} :tls=1")
    c.wait(lambda m, l: m["command"] == "BOUNCER" and "error=" in l, timeout=15)
    c.roundtrip()
    c.send("PRIVMSG BouncerServ :network status")
    c.roundtrip()
    env.settle(1)
    c.send(f"BOUNCER DELNETWORK {netid}")
    c.wait(lambda m, l: m["command"] == "BOUNCER" and m["params"][0] == "DELNETWORK")
    c.roundtrip()


@scenario
def channel_basics(env):
    c = login(env)
    netid = add_network(env, c)
    b = login(env, "bound", user=f"alice/ergo")
    b.send("JOIN #test,#other")
    b.wait(lambda m, l: m["command"] == "366" and m["params"][1] == "#other")
    carol = actor(env, "carol", ["#test"])
    b.wait(lambda m, l: m["command"] == "JOIN" and m["prefix"].startswith("carol"))
    b.send("MODE #test +v carol")
    b.wait(lambda m, l: m["command"] == "MODE")
    b.send("NAMES #test")
    b.send("TOPIC #test :a new topic")
    b.wait(lambda m, l: m["command"] == "TOPIC")
    b.send("TOPIC #test")
    b.send("MODE #test")
    b.send("MODE #test +s")
    b.wait(lambda m, l: m["command"] == "MODE" and "+s" in l)
    carol.send("PRIVMSG #test :hello alice")
    carol.send("NOTICE #test :a notice")
    carol.send("@+draft/react=👍 TAGMSG #test")
    carol.send("PRIVMSG alice :private hi")
    b.wait(lambda m, l: m["command"] == "PRIVMSG" and m["params"][-1] == "private hi")
    b.send("PRIVMSG #test :hi carol")
    b.send("PRIVMSG carol :hi privately")
    b.send("@+typing=active TAGMSG #test")
    carol.wait(lambda m, l: m["command"] == "PRIVMSG" and m["params"][-1] == "hi privately")
    carol.send("AWAY :gone")
    carol.send("AWAY")
    carol.send("NICK carol2")
    b.wait(lambda m, l: m["command"] == "NICK")
    carol.send("PART #test :bye")
    b.wait(lambda m, l: m["command"] == "PART")
    carol.send("JOIN #test")
    b.wait(lambda m, l: m["command"] == "JOIN" and "carol2" in m["prefix"])
    b.send("KICK #test carol2 :out")
    b.wait(lambda m, l: m["command"] == "KICK")
    b.send("PART #other :leaving")
    b.wait(lambda m, l: m["command"] == "PART")
    b.send("JOIN #bad,chan")
    b.send("JOIN ,")
    b.roundtrip()
    carol.send("QUIT :done")
    env.settle(0.5)
    b.send("PRIVMSG BouncerServ :channel status")
    b.roundtrip()


@scenario
def chathistory(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("JOIN #hist")
    b.wait(lambda m, l: m["command"] == "366")
    carol = actor(env, "carol", ["#hist"])
    for i in range(5):
        carol.send(f"PRIVMSG #hist :message {i}")
    carol.send("PRIVMSG alice :dm")
    b.wait(lambda m, l: m["command"] == "PRIVMSG" and m["params"][-1] == "dm")
    time.sleep(1.1)
    b.send("CHATHISTORY LATEST #hist * 3")
    b.send("CHATHISTORY LATEST carol * 10")
    b.send("CHATHISTORY BEFORE #hist timestamp=2099-01-01T00:00:00.000Z 2")
    b.send("CHATHISTORY AFTER #hist timestamp=2000-01-01T00:00:00.000Z 2")
    b.send("CHATHISTORY BETWEEN #hist timestamp=2000-01-01T00:00:00.000Z timestamp=2099-01-01T00:00:00.000Z 10")
    b.send("CHATHISTORY BETWEEN #hist timestamp=2099-01-01T00:00:00.000Z timestamp=2000-01-01T00:00:00.000Z 2")
    b.send("CHATHISTORY TARGETS timestamp=2000-01-01T00:00:00.000Z timestamp=2099-01-01T00:00:00.000Z 10")
    b.send("CHATHISTORY TARGETS timestamp=2099-01-01T00:00:00.000Z timestamp=2000-01-01T00:00:00.000Z 1")
    b.send("CHATHISTORY LATEST #hist * 2000")
    b.send("CHATHISTORY LATEST #hist bogus 2")
    b.send("CHATHISTORY BEFORE #hist msgid=abc 2")
    b.send("CHATHISTORY WHAT #hist")
    b.send("CHATHISTORY LATEST BouncerServ * 10")
    b.send("CHATHISTORY LATEST")
    b.roundtrip()
    b.send("SEARCH :in=#hist;text=message 3")
    b.send("SEARCH :in=#hist;from=carol;limit=2")
    b.send("SEARCH :text=message")
    b.send("SEARCH :in=#hist;limit=x")
    b.send("SEARCH :in=#hist;before=bad")
    b.roundtrip()


@scenario
def read_markers(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b2 = login(env, "bound2", user="alice/ergo", caps=[x for x in ALL_CAPS if x != "draft/read-marker"] + ["soju.im/read"])
    b.send("JOIN #rm")
    b.wait(lambda m, l: m["command"] == "366")
    b2.wait(lambda m, l: m["command"] == "366")
    b.send("MARKREAD #rm")
    b.send("MARKREAD #rm timestamp=2024-01-01T00:00:00.000Z")
    b.send("MARKREAD #rm timestamp=2023-01-01T00:00:00.000Z")
    b.send("MARKREAD #rm timestamp=2099-01-01T00:00:00.000Z")
    b.send("MARKREAD #rm bogus")
    b.send("MARKREAD #rm timestamp=notatime")
    b.send("MARKREAD BouncerServ timestamp=2024-01-01T00:00:00.000Z")
    b.send("MARKREAD")
    b2.send("READ #rm")
    b.roundtrip()
    b2.roundtrip()


@scenario
def detach(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("JOIN #det")
    b.wait(lambda m, l: m["command"] == "366")
    carol = actor(env, "carol", ["#det"])
    b.wait(lambda m, l: m["command"] == "JOIN" and m["prefix"].startswith("carol"))
    b.send("PART #det :detach")
    b.wait(lambda m, l: m["command"] == "PART")
    carol.send("PRIVMSG #det :while detached")
    carol.send("PRIVMSG #det :alice: highlight while detached")
    b.wait(lambda m, l: m["command"] == "NOTICE" and "highlight" in l)
    b.send("PRIVMSG BouncerServ :channel status")
    b.send("PRIVMSG BouncerServ :channel update #det -relay-detached message -detach-after 1h -reattach-on highlight")
    b.roundtrip()
    carol.send("PRIVMSG #det :relayed message")
    b.wait(lambda m, l: m["command"] == "NOTICE" and "relayed message" in l)
    carol.send("PRIVMSG #det :alice: reattach please")
    b.wait(lambda m, l: m["command"] == "JOIN", timeout=5)
    b.roundtrip()
    b.send("PRIVMSG BouncerServ :channel update #det -detached true")
    b.wait(lambda m, l: m["command"] == "PART")
    b.send("JOIN #det")
    b.wait(lambda m, l: m["command"] == "JOIN")
    b.roundtrip()
    b.send("PRIVMSG BouncerServ :channel update #det -relay-detached bogus")
    b.send("PRIVMSG BouncerServ :channel update #det -detach-after -5s")
    b.send("PRIVMSG BouncerServ :channel update #nope -detached true")
    b.send("PRIVMSG BouncerServ :channel create #new -detached true")
    b.send("PRIVMSG BouncerServ :channel delete #new")
    b.roundtrip()


@scenario
def backlog_without_chathistory(env):
    caps = [x for x in ALL_CAPS if x not in ("draft/chathistory", "draft/event-playback")]
    c = login(env)
    add_network(env, c)
    b = login(env, "phone", user="alice@phone/ergo", caps=caps)
    b.send("JOIN #bl")
    b.wait(lambda m, l: m["command"] == "366")
    carol = actor(env, "carol", ["#bl"])
    carol.send("PRIVMSG #bl :first")
    b.wait(lambda m, l: m["command"] == "PRIVMSG" and m["params"][-1] == "first")
    pings = [l for l in b.lines if "soju-msgid-" in l]
    for l in pings:
        b.send("PONG :" + parse(l)["params"][-1])
    b.roundtrip()
    b.send("QUIT")
    env.settle(0.5)
    carol.send("PRIVMSG #bl :while away 1")
    carol.send("PRIVMSG #bl :while away 2")
    carol.send("PRIVMSG alice :dm while away")
    time.sleep(0.5)
    b2 = login(env, "phone2", user="alice@phone/ergo", caps=caps)
    b2.roundtrip()
    env.settle(0.5)


@scenario
def labeled_upstream(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("JOIN #lab")
    b.wait(lambda m, l: m["command"] == "366")
    actor(env, "carol", ["#lab"])
    b.send("@label=who1 WHO #lab")
    b.send("@label=who2 WHO carol %tnuhaf,7")
    b.send("@label=whois WHOIS carol")
    b.send("@label=whoisnick WHOIS nosuchnick")
    b.send("@label=list LIST #lab")
    b.send("@label=names NAMES #lab")
    b.send("@label=topic TOPIC #nosuch")
    b.send("@label=mode MODE #lab")
    b.send("@label=nick NICK alice")
    b.send("@label=join JOIN #lab")
    b.send("@label=priv PRIVMSG carol :hi")
    b.send("@label=raw VERSION")
    b.send("@label=svc PRIVMSG BouncerServ :help")
    b.send("@label=multi PRIVMSG #lab,carol :both")
    b.roundtrip()
    env.settle(1)


@scenario
def metadata(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("METADATA * SUB soju.im/pinned soju.im/muted soju.im/blocked")
    b.send("METADATA #chan SET soju.im/pinned 1")
    b.send("METADATA carol SET soju.im/muted 1")
    b.send("METADATA carol SET soju.im/blocked 1")
    b.send("METADATA carol SET bogus/key 1")
    b.send("METADATA #chan GET soju.im/pinned soju.im/muted nope")
    b.send("METADATA #chan LIST")
    b.send("METADATA #chan CLEAR")
    b.send("METADATA #chan SYNC")
    b.send("METADATA * UNSUB soju.im/muted")
    b.send("METADATA * SUBS")
    b.roundtrip()
    b2 = login(env, "bound2", user="alice/ergo")
    b2.roundtrip()
    carol = actor(env, "carol")
    carol.send("PRIVMSG alice :you blocked me")
    carol.send("INVITE alice #somewhere")
    env.settle(0.8)


@scenario
def monitor(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo", caps=ALL_CAPS + ["extended-monitor"])
    b.send("MONITOR + carol,BouncerServ,dave")
    b.roundtrip()
    env.settle(0.5)
    carol = actor(env, "carol")
    b.wait(lambda m, l: m["command"] == "730")
    b.send("MONITOR L")
    b.send("MONITOR S")
    b.send("MONITOR - dave")
    b.send("MONITOR L")
    b.send("MONITOR C")
    b.send("MONITOR L")
    b.roundtrip()
    carol.send("QUIT")
    env.settle(0.5)


@scenario
def bouncerserv(env):
    c = login(env)
    add_network(env, c)
    cmds = [
        "help", "help network", "help network create", "help nope", "net st", "s", "nope",
        "network create", "network create -addr irc.example.org -bogus", "network create -addr gopher://x",
        "network create -addr 127.0.0.1:1 -name test2 -enabled false -nick tnick -connect-command 'PRIVMSG x :y'",
        "network update test2 -name test3 -certfp 00", "network update test3 -auto-away false",
        "network status", "network quote ergo 'PRIVMSG alice :quoted'", "network quote test3 PING",
        "network delete", "network delete test3", "network delete nope",
        "sasl status -network ergo", "sasl set-plain -network ergo user pass", "sasl status -network ergo",
        "sasl reset -network ergo", "sasl set-plain onlyone", "sasl status",
        "certfp fingerprint -network ergo", "certfp generate -network ergo -key-type ed25519",
        "certfp fingerprint -network ergo", "certfp generate -network ergo -key-type dsa",
        "certfp generate -network ergo -bits 0", "sasl status -network ergo", "sasl reset -network ergo",
        "user status", "user status alice", "user status nobody",
        "user create -username carl -password pw -nick carlnick -realname 'Carl R' -max-networks 3",
        "user create -username carl -password pw", "user create -password x", "user create -username dan",
        "user create -username dan -password x -disable-password",
        "user update carl -admin true -enabled false -max-networks 5", "user update carl -nick x",
        "user update -realname 'Alice R'", "user update -admin true", "user update nobody -admin true",
        "user status", "user delete carl", "user delete carl wrongtoken", "user run bob network status",
        "user run bob help", "user run nobody help", "user run alice network status",
        "channel status", "channel status -network nope", "channel create", "channel delete",
        "server status", "server notice", "server debug maybe", "server bogus",
        "device-certificate status", "device-certificate create -fingerprint 00 -label x",
        "'unterminated", "network create -addr x -name '*'",
    ]
    for cmd in cmds:
        c.send(f"PRIVMSG BouncerServ :{cmd}")
        c.roundtrip()
    tok = None
    for l in c.lines:
        m = parse(l)
        if m["command"] == "PRIVMSG" and m["prefix"].startswith("BouncerServ") and "user delete carl " in m["params"][-1]:
            tok = m["params"][-1].split('"')[1].split()[-1]
    if tok:
        c.send(f"PRIVMSG BouncerServ :user delete carl {tok}")
        c.roundtrip()
    c.send("PRIVMSG BouncerServ :user status")
    c.roundtrip()
    bob = login(env, "bob", user="bob")
    for cmd in ["help", "user status", "user create -username x -password y", "server status",
                "user update alice -admin false", "user delete alice", "network create -addr irc+unix:///tmp/x",
                "network create -addr 127.0.0.1:1 -ignore-limit", "user update -max-networks 2"]:
        bob.send(f"PRIVMSG BouncerServ :{cmd}")
        bob.roundtrip()


@scenario
def caps_negotiation(env):
    c = env.client("c")
    c.send("CAP LS")
    c.send("CAP LS 302")
    c.send("CAP REQ :batch -batch")
    c.send("CAP REQ :-cap-notify")
    c.send("CAP REQ :soju.im/account-required")
    c.send("CAP REQ :BATCH Server-Time")
    c.send("CAP LIST")
    c.send("CAP END")
    c.send("PASS secret")
    c.send("NICK alice")
    c.send("USER alice 0 * :x")
    c.wait(lambda m, l: m["command"] == "001")
    c.roundtrip()
    c.send("CAP REQ :labeled-response")
    c.send("CAP REQ :-server-time")
    c.send("CAP LIST")
    c.roundtrip()
    d = env.client("nocap")
    d.send("PASS secret")
    d.send("NICK alice")
    d.send("USER alice 0 * :x")
    d.wait(lambda m, l: m["command"] == "001")
    d.roundtrip()


@scenario
def message_tags_filtering(env):
    c = login(env)
    add_network(env, c)
    plain = login(env, "plain", user="alice/ergo", caps=["sasl"])
    plain.send("JOIN #tags")
    plain.wait(lambda m, l: m["command"] == "366")
    full = login(env, "full", user="alice/ergo")
    carol = actor(env, "carol", ["#tags"])
    carol.send("@+draft/reply=x PRIVMSG #tags :tagged")
    carol.send("@+typing=active TAGMSG #tags")
    carol.send("AWAY :brb")
    plain.wait(lambda m, l: m["command"] == "PRIVMSG")
    plain.send("PRIVMSG #tags :from plain")
    plain.send("@+x=y PRIVMSG #tags :tags stripped")
    env.settle(5)


@scenario
def setname_nick(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("SETNAME :New Real Name")
    b.wait(lambda m, l: m["command"] == "SETNAME")
    b.send("SETNAME :New Real Name")
    b.send("NICK alice2")
    b.wait(lambda m, l: m["command"] == "NICK")
    b.send("NICK bad!nick")
    b.send("NICK BouncerServ")
    b.send("NICK alice")
    b.wait(lambda m, l: m["command"] == "NICK")
    c.send("NICK alicebouncer")
    c.send("SETNAME :Bouncer Name")
    c.roundtrip()
    env.settle(0.5)


@scenario
def away(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("AWAY :lunch")
    b.send("AWAY")
    b.send("AWAY :again")
    b.roundtrip()
    b.send("QUIT")
    env.settle(0.5)
    carol = actor(env, "carol")
    carol.send("WHOIS alice")
    env.settle(0.5)


@scenario
def event_playback(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo", caps=ALL_CAPS)
    b.send("CAP REQ :draft/event-playback")
    b.wait(lambda m, l: m["command"] == "CAP" and m["params"][1] in ("ACK", "NAK"))
    b.send("JOIN #ev")
    b.wait(lambda m, l: m["command"] == "366")
    carol = actor(env, "carol", ["#ev"])
    carol.send("PRIVMSG #ev :hi")
    carol.send("TOPIC #ev :topic by carol")
    carol.send("PART #ev :bye")
    b.wait(lambda m, l: m["command"] == "PART")
    time.sleep(1.1)
    b.send("CHATHISTORY LATEST #ev * 20")
    b.roundtrip()
    b.send("CAP REQ :-draft/event-playback")
    b.wait(lambda m, l: m["command"] == "CAP")
    b.send("CHATHISTORY LATEST #ev * 20")
    b.roundtrip()


@scenario
def webpush(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("WEBPUSH REGISTER http://push.example.org/x :p256dh=BAAA;auth=AAA")
    b.send("WEBPUSH REGISTER https://push.example.org/x :auth=AAA")
    b.send("WEBPUSH REGISTER https://127.0.0.1/x :p256dh=BNcRdreALRFXTkOOUHK1EtK2wtaz5Ry4YfYCA_0QTpQtUbVlUls0VJXg7A8u-Ts1XbjhazAkj7I99e8QcYP7DkM;auth=tBHItJI5svbpez7KI4CCXg")
    b.send("WEBPUSH UNREGISTER https://push.example.org/unknown")
    b.send("WEBPUSH BOGUS")
    b.send("WEBPUSH")
    b.roundtrip()
    nopush = login(env, "nopush", user="alice/ergo", caps=[x for x in ALL_CAPS if x != "soju.im/webpush"])
    nopush.send("WEBPUSH REGISTER https://x/y :p256dh=a;auth=b")
    nopush.roundtrip()


@scenario
def account_registration(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("@label=reg REGISTER * * hunter2")
    b.wait(lambda m, l: m["command"] in ("REGISTER", "FAIL"), timeout=10)
    b.send("PRIVMSG BouncerServ :sasl status")
    b.roundtrip()
    b.send("REGISTER x")
    b.send("VERIFY x")
    b.roundtrip()
    # Post-registration SASL with the new account
    b.send("AUTHENTICATE PLAIN")
    b.wait(lambda m, l: m["command"] == "AUTHENTICATE")
    b.send("AUTHENTICATE " + base64.b64encode(b"\0alice\0hunter2").decode())
    b.wait(lambda m, l: m["command"] in ("903", "904", "907"), timeout=10)
    env.settle(2)
    b.send("PRIVMSG BouncerServ :sasl status")
    b.roundtrip()


@scenario
def nickserv_autosave(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("PRIVMSG NickServ :REGISTER hunter2")
    b.wait(lambda m, l: m["command"] == "NOTICE" and "NickServ" in m["prefix"], timeout=10)
    env.settle(0.5)
    b.send("PRIVMSG BouncerServ :sasl status")
    b.roundtrip()
    b.send("PRIVMSG NickServ :SET EMAIL x@example.org")
    env.settle(0.5)
    b.send("PRIVMSG BouncerServ :sasl status")
    b.roundtrip()
    b.send("CHATHISTORY LATEST NickServ * 10")
    b.roundtrip()


@scenario
def invite_and_who_cache(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("JOIN #inv")
    b.wait(lambda m, l: m["command"] == "366")
    carol = actor(env, "carol", ["#private"])
    carol.send("INVITE alice #private")
    b.wait(lambda m, l: m["command"] == "INVITE")
    b.send("INVITE carol #inv")
    carol.wait(lambda m, l: m["command"] == "INVITE")
    carol.send("JOIN #inv")
    b.wait(lambda m, l: m["command"] == "JOIN" and m["prefix"].startswith("carol"))
    b.send("WHO #inv")
    b.wait(lambda m, l: m["command"] == "315")
    b.send("WHO #inv")
    b.send("WHO #inv %tcuhnfar,9")
    b.send("WHO carol")
    b.roundtrip()
    env.settle(0.5)


@scenario
def broadcast_and_user_run(env):
    c = login(env)
    bob = login(env, "bob", user="bob")
    c.send("PRIVMSG $soju.test :hello everyone")
    c.send("PRIVMSG $* :hello star")
    c.send("PRIVMSG BouncerServ :server notice 'maintenance soon'")
    c.send("PRIVMSG BouncerServ :user run bob network create -addr 127.0.0.1:1 -name viarun -enabled false")
    c.roundtrip()
    bob.send("PRIVMSG $soju.test :not allowed")
    env.settle(1)


@scenario
def pass_network_autocreate(env):
    c = env.client("c")
    c.send("PASS secret")
    c.send("NICK alice")
    c.send(f"USER alice/127.0.0.1:{env.ports['ergo'] + 30} 0 * :x")
    c.wait(lambda m, l: m["command"] in ("001", "464", "ERROR"), timeout=20)


def ergo_ctl(env, action):
    """Stops or starts the ergo server used by this environment."""
    import subprocess
    port = env.ports["ergo"]
    d = f"{WORK}/env/ergo-{port}"
    if action == "stop":
        # Only the instance started for this environment (pid file written
        # by env-up.sh or below), checked by its working directory
        try:
            with open(f"{d}/pid") as f:
                pid = int(f.read())
            if os.readlink(f"/proc/{pid}/cwd") == d:
                os.kill(pid, signal.SIGTERM)
        except (OSError, ValueError):
            pass
    else:
        p = subprocess.Popen(["bash", "-c", f"cd {d} && exec {WORK}/bin/ergo run --conf ircd.yaml >> ergo.log 2>&1"])
        with open(f"{d}/pid", "w") as f:
            f.write(str(p.pid))
        time.sleep(1)


@scenario
def upstream_reconnect(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("JOIN #rc")
    b.wait(lambda m, l: m["command"] == "366")
    ergo_ctl(env, "stop")
    c.wait(lambda m, l: m["command"] == "BOUNCER" and "state=disconnected" in l, timeout=10)
    c.wait(lambda m, l: m["command"] == "BOUNCER" and "error=" in l, timeout=10)
    b.send("PRIVMSG #rc :while down")
    b.send("PRIVMSG BouncerServ :network status")
    b.roundtrip()
    ergo_ctl(env, "start")
    # The first retry happens immediately, the next one after a minute
    c.wait(lambda m, l: m["command"] == "BOUNCER" and "state=connected" in l, timeout=150)
    b.wait(lambda m, l: m["command"] == "JOIN" and m["params"][0] == "#rc", timeout=15)
    b.wait(lambda m, l: m["command"] == "366", timeout=10)
    b.send("PRIVMSG BouncerServ :network status")
    b.roundtrip()


@scenario
def auto_detach(env):
    c = login(env)
    add_network(env, c)
    b = login(env, "bound", user="alice/ergo")
    b.send("JOIN #ad")
    b.wait(lambda m, l: m["command"] == "366")
    b.send("PRIVMSG BouncerServ :channel update #ad -detach-after 2s")
    b.roundtrip()
    b.wait(lambda m, l: m["command"] == "PART" and m["params"][-1] == "Detach", timeout=10)
    b.send("PRIVMSG BouncerServ :channel status")
    b.roundtrip()


def dump(name):
    fn = [f for f in SCENARIOS if f.__name__ == name][0]
    results, errors = run_scenario(fn)
    for env in ("go", "rs"):
        for client, lines in results[env].items():
            print(f"===== {env} {client}")
            for l in lines:
                print("  " + normalize(l))
        if env in errors:
            print(errors[env])


def main():
    if sys.argv[1:2] == ["--dump"]:
        dump(sys.argv[2])
        return 0
    only = sys.argv[1:]
    failed = []
    for fn in SCENARIOS:
        if only and fn.__name__ not in only:
            continue
        results, errors = run_scenario(fn)
        if not compare(fn.__name__, results, errors):
            failed.append(fn.__name__)
    print(f"\n{len(failed)} failed: {' '.join(failed)}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
