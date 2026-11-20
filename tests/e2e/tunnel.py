"""Open one tunnel the way a subscriber would, bypassing any subscriber.

Usage: tunnel.py HOST PORT CERT_DIR NODE_ID SERVICE

Connects to HOST:PORT with the certificate in CERT_DIR (ca.crt, node.crt,
node.key, from `stouter cert issue`), asks for a tunnel to SERVICE on node
NODE_ID, unsigned (as an older stouter would). Prints "OK" if the tunnel
opened, or "REFUSED <reason>".
"""
import socket
import ssl
import struct
import sys
import time

HOST, PORT, CERT_DIR, NODE_ID, SERVICE = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]


def mp_str(s):
    b = s.encode()
    return (bytes([0xA0 | len(b)]) if len(b) < 32 else bytes([0xD9, len(b)])) + b


def message(fields):
    body = bytes([0x80 | len(fields)])
    for key, value in fields.items():
        body += mp_str(key)
        body += mp_str(value) if isinstance(value, str) else b"\xcf" + struct.pack(">Q", value)
    return struct.pack(">I", len(body)) + body


def read_exact(s, n):
    data = b""
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            raise EOFError("connection closed")
        data += chunk
    return data


ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
ctx.load_verify_locations(f"{CERT_DIR}/ca.crt")
ctx.load_cert_chain(f"{CERT_DIR}/node.crt", f"{CERT_DIR}/node.key")
try:
    s = ctx.wrap_socket(socket.create_connection((HOST, PORT), timeout=20), server_hostname="stouter-node")
    s.sendall(message({
        "type": "tunnelRequest",
        "node_id": NODE_ID,
        "service_name": SERVICE,
        "timestamp_ms": int(time.time() * 1000),
    }))
    status = read_exact(s, 1)
    if status != b"\x00":
        (length,) = struct.unpack(">I", read_exact(s, 4))
        print("REFUSED " + read_exact(s, length).decode(errors="replace"))
        sys.exit(0)
    print("OK")
except Exception as e:  # a refusal mid-handshake, a reset, a timeout
    print(f"REFUSED ({type(e).__name__}: {e})")
