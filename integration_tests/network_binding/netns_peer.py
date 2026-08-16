# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

import selectors
import socket
import struct


LISTEN_IP = "198.18.0.2"
TCP_PORT = 8080
UDP_PORT = 8081
DNS_PORT = 5353


def dns_response(query: bytes) -> bytes:
    if len(query) < 12:
        return b""
    question_count = struct.unpack("!H", query[4:6])[0]
    if question_count != 1:
        return query[:2] + b"\x81\x83" + query[4:6] + b"\x00\x00\x00\x00\x00\x00"
    offset = 12
    while offset < len(query) and query[offset] != 0:
        offset += query[offset] + 1
    question_end = offset + 5
    if question_end > len(query):
        return b""
    header = query[:2] + b"\x81\x80\x00\x01\x00\x01\x00\x00\x00\x00"
    answer = b"\xc0\x0c\x00\x01\x00\x01" + struct.pack("!IH", 30, 4)
    return header + query[12:question_end] + answer + socket.inet_aton(LISTEN_IP)


selector = selectors.DefaultSelector()

tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
tcp.bind((LISTEN_IP, TCP_PORT))
tcp.listen()
tcp.setblocking(False)
selector.register(tcp, selectors.EVENT_READ, "tcp")

udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
udp.bind((LISTEN_IP, UDP_PORT))
udp.setblocking(False)
selector.register(udp, selectors.EVENT_READ, "udp")

dns = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
dns.bind((LISTEN_IP, DNS_PORT))
dns.setblocking(False)
selector.register(dns, selectors.EVENT_READ, "dns")

while True:
    for key, _ in selector.select():
        if key.data == "tcp":
            connection, _ = tcp.accept()
            connection.recv(4096)
            connection.close()
        elif key.data == "udp":
            udp.recvfrom(4096)
        else:
            query, address = dns.recvfrom(4096)
            response = dns_response(query)
            if response:
                dns.sendto(response, address)
