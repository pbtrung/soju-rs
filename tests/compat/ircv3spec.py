"""IRCv3 spec checks against a bouncer (https://ircv3.net/irc/).

Usage: ircv3spec.py <soju port> <ergo port>   (fresh env-up.sh environment)
"""

import base64
import re
import sys
import time

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from ircc import Client, parse  # noqa: E402

PORT, ERGO = int(sys.argv[1]), int(sys.argv[2])
results = []


def check(name, cond, detail=""):
    results.append((name, bool(cond)))
    print(f"{'ok  ' if cond else 'FAIL'} {name}" + ("" if cond else f"  {detail}"))


SERVER_TIME = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$")

# ---- CAP 302 (https://ircv3.net/specs/extensions/capability-negotiation)
c = Client(PORT)
c.send("CAP LS 302")
ls = c.expect("CAP")
caps = ls["params"][-1].split()
check("CAP LS 302 lists values", any("=" in x for x in caps) and "sasl=PLAIN" in caps, caps)
check("CAP LS reply nick is *", ls["params"][0] == "*", ls)
c.send("CAP REQ :batch bogus-cap")
r = c.expect("CAP")
check("CAP REQ is atomic (NAK if any unavailable)", r["params"][1] == "NAK" and r["params"][2] == "batch bogus-cap", r)
c.send("CAP LIST")
r = c.expect("CAP")
check("NAK enables nothing; cap-notify implied by 302", r["params"][1] == "LIST" and r["params"][-1].split() == ["cap-notify"], r)
c.send("CAP REQ :-cap-notify")
r = c.expect("CAP")
check("cap-notify can't be disabled with 302", r["params"][1] == "NAK", r)
c.send("CAP REQ :sasl batch server-time message-tags labeled-response echo-message soju.im/bouncer-networks draft/chathistory draft/read-marker draft/metadata-2")
r = c.expect("CAP")
check("CAP REQ ACK echoes the requested list", r["params"][1] == "ACK", r)

# ---- SASL 3.2 (https://ircv3.net/specs/extensions/sasl-3.2): chunking
c.send("AUTHENTICATE PLAIN")
r = c.expect("AUTHENTICATE")
check("SASL empty challenge is '+'", r["params"] == ["+"], r)
# 300 raw bytes -> exactly 400 base64 chars: must be followed by "+"
user = "alice"
password = "secret"
identity_padding = ""
raw = f"\0{user}\0{password}".encode()
c.send("AUTHENTICATE " + base64.b64encode(raw).decode())
r = c.wait(lambda m, l: m["command"] in ("903", "904"))
check("SASL PLAIN success 903", r["command"] == "903", r)
c.send("NICK alice")
c.send("USER alice 0 * :x")
c.send("CAP END")
welcome = c.expect("001")
check("registration after SASL", welcome["params"][0] == "alice")
burst = c.roundtrip()
check("RPL_LOGGEDIN on bouncer connection", any(" 900 " in l for l in burst), burst[:3])

d = Client(PORT)
d.send("CAP LS 302")
d.send("CAP REQ :sasl")
d.send("AUTHENTICATE PLAIN")
d.wait(lambda m, l: m["command"] == "AUTHENTICATE")
long_password = "x" * (300 - len(f"\0{user}\0"))
enc = base64.b64encode(f"\0{user}\0{long_password}".encode()).decode()
assert len(enc) == 400
d.send("AUTHENTICATE " + enc)
d.send("AUTHENTICATE +")
r = d.wait(lambda m, l: m["command"] in ("903", "904"))
check("SASL 400-byte chunk followed by '+' is reassembled (wrong password -> 904)", r["command"] == "904", r)
d.send("AUTHENTICATE PLAIN")
d.wait(lambda m, l: m["command"] == "AUTHENTICATE")
d.send("AUTHENTICATE *")
r = d.wait(lambda m, l: m["command"] in ("906", "904"))
check("SASL abort 906", r["command"] == "906", r)
d.close()

# ---- labeled-response (https://ircv3.net/specs/extensions/labeled-response)
c.send("@label=one PING x")
r = c.wait(lambda m, l: m["command"] == "PONG")
check("labeled single reply carries label", r["tags"].get("label") == "one", r)
c.send("@label=two BOUNCER LISTNETWORKS")
r = c.wait(lambda m, l: m["command"] in ("BATCH", "ACK", "BOUNCER"))
check("labeled reply with zero-or-more lines: batch or ACK", r["tags"].get("label") == "two", r)
c.roundtrip()
c.send("@label=three WHO alice")
r = c.wait(lambda m, l: m["command"] == "BATCH" and m["params"][0].startswith("+"))
check("labeled multi-line reply is a labeled-response batch", r["tags"].get("label") == "three" and r["params"][1] == "labeled-response", r)
ref = r["params"][0][1:]
r = c.wait(lambda m, l: m["command"] == "352")
check("batched lines carry the batch tag", r["tags"].get("batch") == ref, r)
c.wait(lambda m, l: m["command"] == "BATCH" and m["params"][0] == "-" + ref)
c.send("@label=four AWAY :x")
c.send("AWAY")
c.roundtrip()

# ---- bouncer-networks (https://git.sr.ht/~emersion/soju/tree/master/item/doc/ext/bouncer-networks.md)
c.send(f"BOUNCER ADDNETWORK :name=ergo;host=127.0.0.1;port={ERGO};tls=0")
r = c.wait(lambda m, l: m["command"] == "BOUNCER" and m["params"][0] == "ADDNETWORK")
netid = r["params"][1]
check("BOUNCER ADDNETWORK returns an id", netid.isdigit())
c.send("BOUNCER LISTNETWORKS")
r = c.wait(lambda m, l: m["command"] == "BATCH" and m["params"][0].startswith("+"))
check("LISTNETWORKS batch type", r["params"][1] == "soju.im/bouncer-networks", r)
c.roundtrip()
time.sleep(2)

b = Client(PORT)
b.send("CAP LS 302")
b.send("CAP REQ :sasl batch server-time message-tags labeled-response echo-message draft/chathistory draft/read-marker draft/metadata-2 soju.im/bouncer-networks extended-join account-tag")
b.wait(lambda m, l: m["command"] == "CAP" and m["params"][1] == "ACK")
b.send("AUTHENTICATE PLAIN")
b.wait(lambda m, l: m["command"] == "AUTHENTICATE")
b.send("AUTHENTICATE " + base64.b64encode(b"\0alice\0secret").decode())
b.wait(lambda m, l: m["command"] == "903")
b.send(f"BOUNCER BIND {netid}")
b.send("NICK alice")
b.send("USER alice 0 * :x")
b.send("CAP END")
b.expect("001")
burst = b.roundtrip()
isupport = " ".join(l for l in burst if " 005 " in l)
check("ISUPPORT BOUNCER_NETID", f"BOUNCER_NETID={netid}" in isupport, isupport[:200])
check("ISUPPORT CHATHISTORY=1000", "CHATHISTORY=1000" in isupport)
check("ISUPPORT MSGREFTYPES=timestamp", "MSGREFTYPES=timestamp" in isupport)

# ---- server-time, echo-message, read-marker, chathistory
b.send("JOIN #spec")
join = b.wait(lambda m, l: m["command"] == "JOIN")
check("server-time format", SERVER_TIME.match(join["tags"].get("time", "")), join)
check("extended-join params", len(join["params"]) == 3, join)
mr = b.wait(lambda m, l: m["command"] == "MARKREAD")
check("MARKREAD sent on join", mr["params"] == ["#spec", "*"], mr)
b.wait(lambda m, l: m["command"] == "366")
b.send("@label=e;+draft/reply=x PRIVMSG #spec :echo me")
echo = b.wait(lambda m, l: m["command"] == "PRIVMSG" and m["params"][-1] == "echo me")
check("echo-message echoes with label and client tags", echo["tags"].get("label") == "e" and "+draft/reply" in echo["tags"], echo)
check("echo carries server-time and msgid", SERVER_TIME.match(echo["tags"].get("time", "")) and "msgid" in echo["tags"], echo)
ts = echo["tags"]["time"]
b.send(f"MARKREAD #spec timestamp={ts}")
r = b.wait(lambda m, l: m["command"] == "MARKREAD")
check("MARKREAD reply", r["params"] == ["#spec", f"timestamp={ts}"], r)
time.sleep(1.1)
b.send("CHATHISTORY LATEST #spec * 10")
r = b.wait(lambda m, l: m["command"] == "BATCH" and m["params"][0].startswith("+"))
check("chathistory batch type and target", r["params"][1:] == ["chathistory", "#spec"], r)
msgs = []
while True:
    m = b.wait(lambda m, l: True)
    if m["command"] == "BATCH":
        break
    msgs.append(m)
check("chathistory returns the message", any(m["params"][-1] == "echo me" for m in msgs), msgs)
b.send("CHATHISTORY TARGETS timestamp=2000-01-01T00:00:00.000Z timestamp=2100-01-01T00:00:00.000Z 10")
r = b.wait(lambda m, l: m["command"] == "BATCH" and m["params"][0].startswith("+"))
check("chathistory targets batch type", r["params"][1] == "draft/chathistory-targets", r)
t = b.wait(lambda m, l: m["command"] == "CHATHISTORY")
check("CHATHISTORY TARGETS line", t["params"][0] == "TARGETS" and t["params"][1] == "#spec" and SERVER_TIME.match(t["params"][2]), t)
b.roundtrip()

# ---- metadata-2 (soju.im/pinned etc)
b.send("METADATA #spec SET soju.im/pinned 1")
r = b.wait(lambda m, l: m["command"] == "761")
check("RPL_KEYVALUE format", r["params"] == ["*", "#spec", "soju.im/pinned", "*", "1"], r)
b.roundtrip()

print(f"{len([1 for _, ok in results if ok])}/{len(results)} passed")
sys.exit(0 if all(ok for _, ok in results) else 1)
