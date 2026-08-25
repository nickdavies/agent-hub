#!/usr/bin/env python3
import ipaddress
import os
import socket
import struct
import threading

PUBLIC = os.environ.get("FIXTURE_PUBLIC_IP", "11.77.0.80")
PRIVATE = os.environ.get("FIXTURE_PRIVATE_IP", "10.77.0.80")
COUNTS = {}


def decode_question(packet):
    offset = 12
    labels = []
    while True:
        length = packet[offset]
        offset += 1
        if length == 0:
            break
        labels.append(packet[offset:offset + length].decode("ascii"))
        offset += length
    qtype, qclass = struct.unpack("!HH", packet[offset:offset + 4])
    return ".".join(labels).lower(), qtype, qclass, packet[12:offset + 4]


def encoded_name(name):
    return b"".join(bytes([len(label)]) + label.encode("ascii") for label in name.split(".")) + b"\0"


def rr(record_type, ttl, value):
    if record_type == 1:
        data = ipaddress.IPv4Address(value).packed
    elif record_type == 28:
        data = ipaddress.IPv6Address(value).packed
    elif record_type == 5:
        data = encoded_name(value)
    else:
        raise ValueError(record_type)
    return b"\xc0\x0c" + struct.pack("!HHIH", record_type, 1, ttl, len(data)) + data


def records(name, qtype):
    if name == "public.fixture.net" and qtype == 1:
        return [(1, 1, PUBLIC)]
    if name == "fresh.fixture.net" and qtype == 1:
        return [(1, 1, PUBLIC)]
    if name == "dual.fixture.net":
        return [(1, 1, PUBLIC)] if qtype == 1 else [(28, 1, "2606:4700:10::6814:179a")]
    if name == "private.fixture.net" and qtype == 1:
        return [(1, 1, PRIVATE)]
    if name == "mixed-a.fixture.net" and qtype == 1:
        return [(1, 1, PUBLIC), (1, 1, PRIVATE)]
    if name == "mixed-aaaa.fixture.net":
        if qtype == 1:
            return [(1, 1, PUBLIC)]
        return [(28, 1, "2606:4700:10::6814:179a"), (28, 1, f"::ffff:{PRIVATE}"), (28, 1, "fc00::1")]
    if name == "mapped.fixture.net" and qtype == 28:
        return [(28, 1, f"::ffff:{PRIVATE}")]
    if name == "private-v6.fixture.net" and qtype == 28:
        return [(28, 1, "fc00::1")]
    if name == "cname-private.fixture.net":
        if qtype in (1, 28):
            return [(5, 1, "private.fixture.net"), (1, 1, PRIVATE)]
    if name == "rebind.fixture.net" and qtype == 1:
        count = COUNTS.get(name, 0)
        COUNTS[name] = count + 1
        return [(1, 1, PUBLIC if count == 0 else PRIVATE)]
    if name == "truncated.fixture.net" and qtype == 1:
        return [(1, 1, PUBLIC)]
    return []


def response(packet, tcp):
    if len(packet) < 12:
        return None
    name, qtype, qclass, question = decode_question(packet)
    request_id = packet[:2]
    if name == "timeout.fixture.net":
        return None
    if name == "malformed.fixture.net":
        return b"malformed"
    if name == "truncated.fixture.net" and not tcp:
        return request_id + b"\x83\x80" + struct.pack("!HHHH", 1, 0, 0, 0) + question
    answers = records(name, qtype) if qclass == 1 else []
    flags = b"\x81\x80" if answers else b"\x81\x83"
    header = request_id + flags + struct.pack("!HHHH", 1, len(answers), 0, 0)
    return header + question + b"".join(rr(*answer) for answer in answers)


def udp_server():
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("0.0.0.0", 53))
    while True:
        packet, peer = sock.recvfrom(65535)
        answer = response(packet, False)
        if answer is not None:
            sock.sendto(answer, peer)


def tcp_client(conn):
    with conn:
        length = conn.recv(2)
        if len(length) != 2:
            return
        expected = struct.unpack("!H", length)[0]
        packet = b""
        while len(packet) < expected:
            data = conn.recv(expected - len(packet))
            if not data:
                return
            packet += data
        answer = response(packet, True)
        if answer is not None:
            conn.sendall(struct.pack("!H", len(answer)) + answer)


def tcp_server():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("0.0.0.0", 53))
    sock.listen()
    while True:
        conn, _ = sock.accept()
        threading.Thread(target=tcp_client, args=(conn,), daemon=True).start()


threading.Thread(target=tcp_server, daemon=True).start()
udp_server()
