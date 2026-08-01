#!/usr/bin/env python3
"""Decode a Supvan/Katasymbol print session out of an Android btsnoop_hci log.

Pipeline: btsnoop record -> HCI ACL -> L2CAP reassembly -> RFCOMM UIH payload
-> Supvan framing (0x7E 0x5A) -> LZMA print payload -> print buffers.

The point is to diff the vendor Android app's wire bytes against what
supvan-proto emits, field by field, for a printer that runs a full print cycle
but marks nothing.

Usage: btsnoop_supvan.py <btsnoop_hci.log> [--bdaddr AA:BB:CC:DD:EE:FF]
"""

import argparse
import lzma
import struct
import sys
from collections import defaultdict

# --- Supvan framing constants (mirrors crates/supvan-proto/src/cmd.rs) -------
MAGIC1, MAGIC2 = 0x7E, 0x5A
PROTO_ID = 0x10
TYPE_CMD_OUT, TYPE_DATA, TYPE_CMD_IN = 0x01, 0x02, 0x03

CMD_NAMES = {
    0x10: "BUF_FULL", 0x11: "INQUIRY_STA", 0x12: "CHECK_DEVICE",
    0x13: "START_PRINT", 0x14: "STOP_PRINT", 0x16: "RD_DEV_NAME",
    0x17: "READ_REV", 0x19: "CHECK_RIB", 0x1A: "SET_MAT",
    0x2E: "PAPER_SKIP", 0x30: "RETURN_MAT", 0x5C: "NEXT_ZIPPEDBULK",
    0x5D: "SET_RFID_DATA",
}

# Print-buffer sizes we know of, mirroring supvan_proto::profile::PrintProfile:
# 4096 for the T50/T80/G/TP families, 4000 for the BT-only E-series. Which one
# a capture uses is not stated on the wire, so it is inferred per stream.
PRINT_BUF_SIZES = (4096, 4000)
PRINT_BUF_HEADER = 14


def infer_buf_size(raw):
    """Pick the print-buffer size a decompressed payload was built with.

    A buffer's header carries its own column count and bytes-per-line, and the
    two must account for no more than the buffer's data area. That is enough to
    tell 4000 apart from 4096: reading E-series buffers at 4096 lands the second
    header 96 bytes into image data, where those fields are garbage.
    """
    for size in PRINT_BUF_SIZES:
        if not raw or len(raw) % size:
            continue
        ok = True
        for n in range(len(raw) // size):
            b = raw[n * size:(n + 1) * size]
            cols = struct.unpack("<H", b[4:6])[0]
            per_line = b[6]
            if per_line == 0 or cols == 0 or cols * per_line > size - PRINT_BUF_HEADER:
                ok = False
                break
        if ok:
            return size
    return None


def read_btsnoop(path):
    """Yield (is_from_host, timestamp_us, hci_payload) per btsnoop record."""
    with open(path, "rb") as fh:
        hdr = fh.read(16)
        if hdr[:8] != b"btsnoop\x00":
            sys.exit(f"{path}: not a btsnoop file")
        datalink = struct.unpack(">I", hdr[12:16])[0]
        while True:
            rec = fh.read(24)
            if len(rec) < 24:
                return
            _olen, ilen, flags, _drops, ts = struct.unpack(">IIIIq", rec)
            data = fh.read(ilen)
            if len(data) < ilen:
                return
            # flags bit0: 0 = host->controller (sent), 1 = received.
            is_from_host = not (flags & 0x01)
            if datalink in (1002, 1003):  # H4: leading packet-type byte
                if not data:
                    continue
                ptype, data = data[0], data[1:]
                if ptype != 0x02:  # keep ACL only
                    continue
            yield is_from_host, ts, data


def l2cap_streams(records):
    """Reassemble ACL fragments into complete L2CAP PDUs.

    Yields (is_from_host, ts, cid, payload).
    """
    pending = {}  # (handle, dir) -> [need, buf, ts]
    for is_from_host, ts, acl in records:
        if len(acl) < 4:
            continue
        hf, dlen = struct.unpack("<HH", acl[:4])
        handle, pb = hf & 0x0FFF, (hf >> 12) & 0x03
        body = acl[4:4 + dlen]
        key = (handle, is_from_host)
        if pb == 0x01:  # continuation
            st = pending.get(key)
            if not st:
                continue
            st[1] += body
        else:  # start of a new L2CAP PDU
            if len(body) < 4:
                continue
            plen, cid = struct.unpack("<HH", body[:4])
            pending[key] = [plen, bytearray(body[4:]), ts, cid]
            st = pending[key]
        st = pending.get(key)
        if st and len(st[1]) >= st[0]:
            yield is_from_host, st[2], st[3], bytes(st[1][:st[0]])
            del pending[key]


def rfcomm_payloads(pdus):
    """Extract RFCOMM UIH user data. Yields (is_from_host, ts, dlci, data)."""
    for is_from_host, ts, _cid, p in pdus:
        if len(p) < 4:
            continue
        addr, ctrl = p[0], p[1]
        if ctrl & ~0x10 != 0xEF:  # UIH, ignoring the P/F bit
            continue
        dlci = addr >> 2
        if dlci == 0:  # control channel, not user data
            continue
        i = 2
        ln = p[i]
        if ln & 0x01:
            ln >>= 1
            i += 1
        else:
            ln = (p[i] >> 1) | (p[i + 1] << 7)
            i += 2
        if ctrl & 0x10:  # P/F set => credit-based flow control byte
            i += 1
        body = p[i:i + ln]
        if body:
            yield is_from_host, ts, dlci, body


def decode_supvan(stream, label):
    """Walk a reassembled byte stream and report Supvan frames."""
    print(f"\n=== {label} ({len(stream)} bytes) ===")
    i, bulk = 0, bytearray()
    while i < len(stream) - 5:
        if stream[i] != MAGIC1 or stream[i + 1] != MAGIC2:
            i += 1
            continue
        plen = struct.unpack("<H", stream[i + 2:i + 4])[0]
        ptype = stream[i + 5] if i + 5 < len(stream) else 0
        if ptype == TYPE_DATA:
            pkt = stream[i + 6:i + 512]
            if len(pkt) >= 6 and pkt[0] == 0xAA:
                idx, tot = pkt[4], pkt[5]
                bulk += pkt[6:506]
                print(f"  DATA packet {idx + 1}/{tot}")
            i += 512
            continue
        # cmd.rs layout: [8..10]=checksum, [10]=0x00 [11]=0x01,
        # [12..14]=param/block_size, [14..16]=block_count.
        cmd = stream[i + 7] if i + 7 < len(stream) else 0
        # `len16` counts the payload after the 2-byte magic and its own 2
        # bytes, so a frame is 4 + plen on the wire — a 0x0C command frame is
        # the 16 bytes documented in docs/E10-PROTOCOL.md. Advancing by
        # 6 + plen walks 2 bytes into the next frame, and for any plen other
        # than 0x0C (a 0x16 status response, say) the walk never resynchronises
        # on the following magic.
        flen = 4 + plen
        frame = stream[i:i + flen]
        chk = struct.unpack("<H", stream[i + 8:i + 10])[0] if i + 10 <= len(stream) else 0
        if plen == 0x0C and i + 16 <= len(stream):
            param = struct.unpack("<H", stream[i + 12:i + 14])[0]
            count = struct.unpack("<H", stream[i + 14:i + 16])[0]
            extra = ""
        else:
            param = count = 0
            tail = stream[i + 12:i + flen]
            printable = "".join(chr(c) if 32 <= c < 127 else "." for c in tail)
            extra = f" tail={tail.hex()} ascii={printable!r}"
        name = CMD_NAMES.get(cmd, f"0x{cmd:02X}")
        print(f"  {name:<16} param={param:<6} count={count:<4} chk={chk}"
              f"  {frame.hex()}{extra}")
        i += flen
    return bytes(bulk)


def dump_print_buffers(blob):
    """Decompress the bulk payload and dump each print buffer's header."""
    print(f"\n=== bulk payload: {len(blob)} bytes ===")
    if not blob:
        return
    print(f"  LZMA props byte : 0x{blob[0]:02X}")
    if len(blob) >= 5:
        print(f"  dict size       : {struct.unpack('<I', blob[1:5])[0]}")
    if len(blob) >= 13:
        print(f"  uncompressed len: {struct.unpack('<Q', blob[5:13])[0]}")
    try:
        raw = lzma.decompress(blob, format=lzma.FORMAT_ALONE)
    except Exception as exc:  # noqa: BLE001 - diagnostic tool
        try:
            patched = blob[:5] + b"\xff" * 8 + blob[13:]
            raw = lzma.decompress(patched, format=lzma.FORMAT_ALONE)
        except Exception:
            print(f"  !! LZMA decode failed: {exc}")
            return
    buf_size = infer_buf_size(raw)
    if buf_size is None:
        print(f"  decompressed    : {len(raw)} bytes "
              f"(no known buffer size divides this; tried {PRINT_BUF_SIZES})")
        return
    print(f"  decompressed    : {len(raw)} bytes "
          f"({len(raw) // buf_size} print buffers of {buf_size})")
    for n in range(len(raw) // buf_size):
        b = raw[n * buf_size:(n + 1) * buf_size]
        chk = struct.unpack("<H", b[0:2])[0]
        page = struct.unpack("<H", b[2:4])[0]
        cols = struct.unpack("<H", b[4:6])[0]
        per_line = b[6]
        mt = struct.unpack("<H", b[8:10])[0]
        mb = struct.unpack("<H", b[10:12])[0]
        dens = b[12]
        ink = sum(bin(x).count("1") for x in b[PRINT_BUF_HEADER:])
        print(f"  buf[{n}] chk=0x{chk:04X} page_reg=0x{page:04X} cols={cols} "
              f"per_line_byte={per_line} margin_top={mt} margin_bottom={mb} "
              f"density={dens} ink_bits={ink}")
        b0, b1 = b[2], b[3]
        print(f"         page_st={bool(b0 & 0x02)} page_end={bool(b0 & 0x04)} "
              f"prt_end={bool(b0 & 0x08)} cut={(b0 >> 4) & 7} "
              f"first_cut={b1 & 3} nodu(density)={(b1 >> 2) & 0x0F} "
              f"mat={(b1 >> 6) & 3}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--bdaddr", help="printer address (informational)")
    args = ap.parse_args()

    pdus = l2cap_streams(read_btsnoop(args.log))
    by_dir = defaultdict(bytearray)
    for is_from_host, _ts, dlci, body in rfcomm_payloads(pdus):
        by_dir[(is_from_host, dlci)] += body

    if not by_dir:
        sys.exit("no RFCOMM user data found — was the snoop log captured "
                 "with Bluetooth toggled off/on before printing?")

    for (is_from_host, dlci), stream in sorted(by_dir.items()):
        direction = "phone -> printer" if is_from_host else "printer -> phone"
        blob = decode_supvan(bytes(stream), f"DLCI {dlci}  {direction}")
        if is_from_host:
            dump_print_buffers(blob)


if __name__ == "__main__":
    main()
