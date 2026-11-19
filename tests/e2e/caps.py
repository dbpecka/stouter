"""Per-peer connection caps, under load, against a running node.

Usage: caps.py HOST PORT PEER1_CERT_DIR PEER2_CERT_DIR

Each cert dir holds ca.crt, node.crt and node.key (from `stouter cert issue`).
Speaks the mesh's own framing: a 4-byte big-endian length, then a
MessagePack map. Prints PASS/FAIL lines; exits 1 if any check fails.
"""
import resource
import select
import socket
import ssl
import struct
import sys
import time

HOST, PORT, PEER1, PEER2 = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
# Limits in src/gossip/mod.rs, src/node/relay.rs and src/node/mod.rs.
MAX_IDLE_PER_PEER = 64
MAX_MUX_SESSIONS_PER_PEER = 16
MAX_IDLE_REVERSE_PER_NODE = 1024
MAX_HANDSHAKES = 512
MAX_HANDSHAKES_PER_IP = 128

soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
resource.setrlimit(resource.RLIMIT_NOFILE, (min(hard, 8192), hard))
failed = False


def report(ok, what):
    global failed
    print(("PASS: " if ok else "FAIL: ") + what, flush=True)
    failed |= not ok


def mp_str(s):
    b = s.encode()
    return (bytes([0xA0 | len(b)]) if len(b) < 32 else bytes([0xD9, len(b)])) + b


def message(fields):
    body = bytes([0x80 | len(fields)])
    for key, value in fields.items():
        body += mp_str(key)
        body += mp_str(value) if isinstance(value, str) else b"\xcf" + struct.pack(">Q", value)
    return struct.pack(">I", len(body)) + body


def tls(cert_dir, source=None):
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.load_verify_locations(f"{cert_dir}/ca.crt")
    ctx.load_cert_chain(f"{cert_dir}/node.crt", f"{cert_dir}/node.key")
    tcp = socket.create_connection((HOST, PORT), timeout=10, source_address=(source, 0) if source else None)
    return ctx.wrap_socket(tcp, server_hostname="stouter-node")


def plain_still_open(socks, settle=1.0):
    """still_open for plain sockets, all at once: a closed one is readable (EOF)."""
    time.sleep(settle)
    readable, _, _ = select.select(socks, [], [], 0)
    closed = 0
    for s in readable:
        try:
            if s.recv(1) == b"":
                closed += 1
        except OSError:
            closed += 1
    return len(socks) - closed


def still_open(socks, settle=1.5):
    """How many the server has kept: a closed one reads EOF (or errors)."""
    time.sleep(settle)
    alive = 0
    for s in socks:
        s.settimeout(0.05)
        try:
            data = s.recv(1)
            if data:
                alive += 1  # sent something: it's open
        except (socket.timeout, ssl.SSLWantReadError, TimeoutError):
            alive += 1
        except (OSError, ssl.SSLError):
            pass
    return alive


def close(socks):
    for s in socks:
        try:
            s.close()
        except OSError:
            pass
    time.sleep(0.5)


def opened(n, cert_dir, first=None):
    socks = []
    for _ in range(n):
        try:
            s = tls(cert_dir)
            if first is not None:
                s.sendall(first)
            socks.append(s)
        except (OSError, ssl.SSLError):
            pass
    return socks


# 1. Connections that never send their first message.
idle = opened(MAX_IDLE_PER_PEER + 16, PEER1)
kept = still_open(idle)
report(kept == MAX_IDLE_PER_PEER, f"one peer holds at most {MAX_IDLE_PER_PEER} idle connections ({kept} of {len(idle)} kept)")
other = opened(1, PEER2)
report(still_open(other) == 1, "another peer still gets in")
close(idle + other)

# 2. Mux sessions.
mux = opened(MAX_MUX_SESSIONS_PER_PEER + 4, PEER1, message({"type": "muxTunnel"}))
kept = still_open(mux)
report(kept == MAX_MUX_SESSIONS_PER_PEER, f"one peer holds at most {MAX_MUX_SESSIONS_PER_PEER} mux sessions ({kept} kept)")
close(mux)

# 3. Idle reverse connections (a NAT'd node registering with its relay).
def registration():
    return message({"type": "reverseRegistration", "node_id": "peer1", "timestamp_ms": int(time.time() * 1000)})

reverse = []
for _ in range(MAX_IDLE_REVERSE_PER_NODE + 16):
    reverse += opened(1, PEER1, registration())
kept = still_open(reverse, settle=3)
report(kept == MAX_IDLE_REVERSE_PER_NODE, f"one node holds at most {MAX_IDLE_REVERSE_PER_NODE} idle reverse connections ({kept} kept)")
close(reverse)

# 4. A flood of connections that never start a TLS handshake.
flood = []
for _ in range(MAX_HANDSHAKES_PER_IP + 200):
    try:
        flood.append(socket.create_connection((HOST, PORT), timeout=5))
    except OSError:
        pass
kept = still_open(flood)
report(kept <= MAX_HANDSHAKES_PER_IP, f"at most {MAX_HANDSHAKES_PER_IP} handshakes in progress from one address ({kept} kept)")
close(flood)
after = opened(1, PEER2)
report(still_open(after) == 1, "after the flood, peers get in again")
close(after)

# 5. Stalled connections from several addresses holding every handshake
# slot (on loopback, any 127.x address is this host): a peer still gets in,
# and they only displace one another.
if HOST == "127.0.0.1":
    stalled = []
    for i in range(MAX_HANDSHAKES):
        try:
            source = f"127.0.0.{10 + i % 4}"
            stalled.append(socket.create_connection((HOST, PORT), timeout=5, source_address=(source, 0)))
        except OSError:
            pass
    time.sleep(0.5)
    try:
        peer = [tls(PEER2, source="127.0.0.20")]
    except (OSError, ssl.SSLError) as e:
        print(f"peer connection: {e}")
        peer = []
    report(len(peer) == 1 and still_open(peer, settle=0.5) == 1,
           f"a peer gets in while {len(stalled)} stalled connections from 4 addresses hold every handshake slot")
    kept = plain_still_open(stalled, settle=0)
    report(kept == MAX_HANDSHAKES - 1, f"and only one of those is dropped for it ({kept} of {len(stalled)} kept)")
    close(stalled + peer)

sys.exit(1 if failed else 0)
