"""echo.py PORT TAG: answer each connection with TAG:<what it sent>, once it half-closes."""
import socket
import sys
import threading

port, tag = int(sys.argv[1]), sys.argv[2].encode()
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(64)


def handle(conn):
    data = b""
    while chunk := conn.recv(65536):
        data += chunk
    conn.sendall(tag + b":" + data)
    conn.close()


while True:
    conn, _ = srv.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
