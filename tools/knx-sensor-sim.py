#!/usr/bin/env python3
"""KNXnet/IP gateway with sensors behind it, for testing weather-station-knx.

The simulator shipped with knx-pico is a loopback echo: it only reflects group
*writes* back as indications, and its cEMI builder handles a boolean. A
read-only station never writes, so that simulator can never make it publish.
This one is the other half — it *originates* traffic, the way a KNX sensor
sending cyclically does:

    CONNECT_REQUEST        → CONNECT_RESPONSE
    CONNECTIONSTATE_REQUEST → CONNECTIONSTATE_RESPONSE   (the station's keepalive)
    DISCONNECT_REQUEST     → DISCONNECT_RESPONSE
    TUNNELING_REQUEST      → TUNNELING_ACK               (nothing should arrive)
    …then TUNNELING_INDICATION frames on a timer, one per configured sensor.

Usage:

    # 21.5 °C on 9/1/0 and 48 % on 9/1/1, every 10 seconds
    python3 tools/knx-sensor-sim.py --ga 9/1/0=21.5 --ga 9/1/1=48 --interval 10

    # humidity as a 1-byte percentage instead (DPT 5.001)
    python3 tools/knx-sensor-sim.py --ga 9/1/0=21.5 --ga 9/1/1=48:5.001

    # every 5th telegram on 9/1/0 is a truncated one, to exercise the
    # station's "reject, never coerce" path
    python3 tools/knx-sensor-sim.py --ga 9/1/0=21.5 --ga 9/1/1=48 --malformed-every 5

Values drift by ±`--drift` per cycle so a stalled record is visible; the
default of 0 keeps them fixed, which is what an assertion wants.
"""

import argparse
import random
import socket
import struct
import sys
import time
from datetime import datetime

# KNXnet/IP service type identifiers
CONNECT_REQUEST = 0x0205
CONNECT_RESPONSE = 0x0206
CONNECTIONSTATE_REQUEST = 0x0207
CONNECTIONSTATE_RESPONSE = 0x0208
DISCONNECT_REQUEST = 0x0209
DISCONNECT_RESPONSE = 0x020A
TUNNELING_REQUEST = 0x0420  # also the service type of a gateway indication
TUNNELING_ACK = 0x0421

E_NO_ERROR = 0x00

# Individual address the simulated sensors send from (1.1.250).
SOURCE_ADDRESS = 0x11FA


# ─────────────────────────────────────────────────────────────────────────────
# Encoding
# ─────────────────────────────────────────────────────────────────────────────


def encode_dpt9(value: float) -> bytes:
    """2-byte KNX float (DPT 9.xxx): SEEE EMMM MMMM MMMM, value = 0.01·M·2^E.

    Mirrors knx-pico's decoder, which reads the mantissa as an 11-bit two's
    complement and the exponent as 4 unsigned bits.
    """
    if value == 0.0:
        return b"\x00\x00"

    mantissa = value * 100.0
    exponent = 0
    while mantissa < -2048.0 or mantissa > 2047.0:
        mantissa /= 2.0
        exponent += 1
        if exponent > 15:
            raise ValueError(f"{value} is out of range for a DPT 9 value")

    mantissa = int(round(mantissa))
    if mantissa < 0:
        raw = 0x8000 | (exponent << 11) | ((mantissa + 2048) & 0x07FF)
    else:
        raw = (exponent << 11) | (mantissa & 0x07FF)
    return struct.pack(">H", raw)


def encode_dpt5_percent(value: float) -> bytes:
    """1-byte scaled percentage (DPT 5.001): 0–100 % over the raw range 0–255."""
    clamped = min(100.0, max(0.0, value))
    return bytes([int(round(clamped * 255.0 / 100.0))])


ENCODERS = {
    "9.001": encode_dpt9,
    "9.007": encode_dpt9,
    "5.001": encode_dpt5_percent,
}


def parse_group_address(text: str) -> int:
    """3-level `main/middle/sub` → the 16-bit wire form."""
    parts = text.split("/")
    if len(parts) != 3:
        raise ValueError(f"'{text}' is not a 3-level group address")
    main, middle, sub = (int(p) for p in parts)
    if not (0 <= main <= 31 and 0 <= middle <= 7 and 0 <= sub <= 255):
        raise ValueError(f"'{text}' has a level out of range")
    return (main << 11) | (middle << 8) | sub


def build_cemi(group_address: int, payload: bytes) -> bytes:
    """An L_Data.ind carrying a GroupValueWrite — what a sensor puts on the bus.

    The NPDU length octet counts the APCI octet plus the data octets, so it is
    `len(payload) + 1`; a station that reads it any other way loses the value
    of a single-octet datapoint.
    """
    cemi = bytearray()
    cemi.append(0x29)  # L_Data.ind
    cemi.append(0x00)  # additional info length
    cemi.append(0xBC)  # control field 1: standard frame, low priority
    cemi.append(0xE0)  # control field 2: group address, hop count 6
    cemi.extend(struct.pack(">H", SOURCE_ADDRESS))
    cemi.extend(struct.pack(">H", group_address))

    if len(payload) == 1 and payload[0] < 64:
        # 6-bit encoding: the value rides in the APCI octet (DPT1 and friends).
        cemi.append(0x01)
        cemi.append(0x00)  # TPCI
        cemi.append(0x80 | (payload[0] & 0x3F))
    else:
        cemi.append(len(payload) + 1)
        cemi.append(0x00)  # TPCI
        cemi.append(0x80)  # APCI: GroupValueWrite
        cemi.extend(payload)
    return bytes(cemi)


def build_frame(service_type: int, body: bytes) -> bytes:
    return struct.pack(">BBHH", 0x06, 0x10, service_type, 6 + len(body)) + body


# ─────────────────────────────────────────────────────────────────────────────
# Sensors
# ─────────────────────────────────────────────────────────────────────────────


class Sensor:
    """One group address sending a value cyclically."""

    def __init__(self, spec: str):
        # GA=VALUE[:DPT]
        address, _, rest = spec.partition("=")
        if not rest:
            raise ValueError(f"--ga '{spec}' is not of the form 9/1/0=21.5[:9.001]")
        value, _, dpt = rest.partition(":")
        dpt = dpt or "9.001"
        if dpt not in ENCODERS:
            raise ValueError(
                f"--ga '{spec}': unknown DPT '{dpt}' — "
                f"one of {', '.join(sorted(ENCODERS))}"
            )

        self.text = address.strip()
        self.address = parse_group_address(self.text)
        self.value = float(value)
        self.dpt = dpt
        self.sent = 0

    @property
    def can_be_malformed(self) -> bool:
        """Whether truncation produces a telegram the station must reject.

        Only for multi-octet datapoint types. A 1-octet DPT truncated to
        nothing frames as a valid 6-bit telegram carrying 0, which is a
        legitimate reading — the station would be right to accept it, so
        sending it would test nothing and publish a bogus 0 into the mesh.
        """
        return len(ENCODERS[self.dpt](self.value)) > 1

    def next_payload(self, drift: float, malformed: bool) -> bytes:
        self.sent += 1
        if drift:
            self.value += random.uniform(-drift, drift)
        payload = ENCODERS[self.dpt](self.value)
        if malformed:
            # One octet where the datapoint type needs two: undecodable, and
            # the station must drop it rather than coerce it to 0.
            payload = payload[:1]
        return payload


# ─────────────────────────────────────────────────────────────────────────────
# Gateway
# ─────────────────────────────────────────────────────────────────────────────


class SensorGateway:
    def __init__(self, port, sensors, interval, drift, malformed_every, verbose):
        self.sensors = sensors
        self.interval = interval
        self.drift = drift
        self.malformed_every = malformed_every
        self.verbose = verbose

        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", port))
        self.port = port

        # channel_id → [client address, outbound sequence counter]
        self.channels = {}
        self.next_channel = 1

    def log(self, message):
        stamp = datetime.now().strftime("%H:%M:%S.%f")[:-3]
        print(f"[{stamp}] {message}", flush=True)

    def debug(self, message):
        if self.verbose:
            self.log(message)

    # ── request handling ────────────────────────────────────────────────────

    def handle(self, service_type, body, client):
        if service_type == CONNECT_REQUEST:
            channel_id = self.next_channel
            self.next_channel = self.next_channel % 255 + 1
            self.channels[channel_id] = [client, 0]
            self.log(f"CONNECT from {client[0]}:{client[1]} → channel {channel_id}")

            # channel + status, then the data endpoint HPAI and the CRD.
            reply = struct.pack("BB", channel_id, E_NO_ERROR)
            reply += struct.pack(">BB4sH", 0x08, 0x01, b"\x00\x00\x00\x00", 0)
            reply += struct.pack(">BBH", 0x04, 0x04, 0x0200)
            return build_frame(CONNECT_RESPONSE, reply)

        if service_type == CONNECTIONSTATE_REQUEST and len(body) >= 2:
            channel_id = body[0]
            self.debug(f"CONNECTIONSTATE channel {channel_id}")
            status = E_NO_ERROR if channel_id in self.channels else 0x21
            return build_frame(
                CONNECTIONSTATE_RESPONSE, struct.pack("BB", channel_id, status)
            )

        if service_type == DISCONNECT_REQUEST and len(body) >= 2:
            channel_id = body[0]
            self.channels.pop(channel_id, None)
            self.log(f"DISCONNECT channel {channel_id}")
            return build_frame(
                DISCONNECT_RESPONSE, struct.pack("BB", channel_id, E_NO_ERROR)
            )

        if service_type == TUNNELING_REQUEST and len(body) >= 4:
            # The station is read-only, so this is unexpected — ACK it anyway
            # and say so, since it means the station grew a writer.
            connection_header = body[:4]
            self.log(
                f"TUNNELING_REQUEST from the station (channel {body[1]}, "
                f"seq {body[2]}) — this station should not be writing"
            )
            return build_frame(
                TUNNELING_ACK, connection_header + struct.pack("B", E_NO_ERROR)
            )

        if service_type == TUNNELING_ACK and len(body) >= 4:
            self.debug(f"TUNNELING_ACK channel {body[1]} seq {body[2]}")
            return None

        self.debug(f"ignoring service type 0x{service_type:04X}")
        return None

    # ── sending ─────────────────────────────────────────────────────────────

    def send_readings(self):
        if not self.channels:
            self.debug("no station connected, nothing to send")
            return

        for sensor in self.sensors:
            malformed = bool(
                self.malformed_every
                and sensor.can_be_malformed
                and (sensor.sent + 1) % self.malformed_every == 0
            )
            payload = sensor.next_payload(self.drift, malformed)
            cemi = build_cemi(sensor.address, payload)

            for channel_id, (client, sequence) in list(self.channels.items()):
                header = struct.pack("BBBB", 0x04, channel_id, sequence, 0x00)
                self.sock.sendto(
                    build_frame(TUNNELING_REQUEST, header + cemi), client
                )
                self.channels[channel_id][1] = (sequence + 1) % 256

            note = " (deliberately malformed)" if malformed else ""
            self.log(
                f"→ {sensor.text} = {sensor.value:.2f} "
                f"(DPT {sensor.dpt}, {payload.hex() or 'empty'}){note}"
            )

    # ── main loop ───────────────────────────────────────────────────────────

    def run(self):
        print("=== KNX sensor simulator ===")
        print(f"Listening on 0.0.0.0:{self.port} (gateway address 1.1.250)")
        for sensor in self.sensors:
            note = ""
            if self.malformed_every and not sensor.can_be_malformed:
                note = " — --malformed-every does not apply to a 1-octet DPT"
            print(f"  {sensor.text} = {sensor.value} as DPT {sensor.dpt}{note}")
        print(f"Sending every {self.interval}s once a station connects.")
        print("Press Ctrl+C to stop\n")

        next_send = time.monotonic() + self.interval
        try:
            while True:
                timeout = max(0.0, next_send - time.monotonic())
                self.sock.settimeout(timeout)
                try:
                    data, client = self.sock.recvfrom(2048)
                except socket.timeout:
                    self.send_readings()
                    next_send = time.monotonic() + self.interval
                    continue

                if len(data) < 6:
                    continue
                _, _, service_type, total_length = struct.unpack(">BBHH", data[:6])
                body = data[6:total_length] if total_length >= 6 else data[6:]

                reply = self.handle(service_type, body, client)
                if reply:
                    self.sock.sendto(reply, client)
        except KeyboardInterrupt:
            print("\nShutting down…")
        finally:
            self.sock.close()


def main():
    parser = argparse.ArgumentParser(
        description="KNXnet/IP gateway with cyclically sending sensors behind it",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("--port", type=int, default=3671, help="UDP port (default 3671)")
    parser.add_argument(
        "--ga",
        action="append",
        default=[],
        metavar="GA=VALUE[:DPT]",
        help="a sensor, e.g. 9/1/0=21.5 or 9/1/1=48:5.001 "
        "(DPT one of 9.001, 9.007, 5.001; default 9.001). Repeatable.",
    )
    parser.add_argument(
        "--interval", type=float, default=10.0, help="seconds between cycles (default 10)"
    )
    parser.add_argument(
        "--drift",
        type=float,
        default=0.0,
        help="random walk applied to each value per cycle (default 0, values stay fixed)",
    )
    parser.add_argument(
        "--malformed-every",
        type=int,
        default=0,
        metavar="N",
        help="send a truncated telegram every Nth cycle, to exercise the "
        "station's rejection path. Applies to multi-octet DPTs only "
        "(default 0, never)",
    )
    parser.add_argument("-v", "--verbose", action="store_true", help="log every datagram")
    args = parser.parse_args()

    if not args.ga:
        parser.error("give at least one --ga, e.g. --ga 9/1/0=21.5")

    try:
        sensors = [Sensor(spec) for spec in args.ga]
    except ValueError as e:
        parser.error(str(e))

    SensorGateway(
        port=args.port,
        sensors=sensors,
        interval=args.interval,
        drift=args.drift,
        malformed_every=args.malformed_every,
        verbose=args.verbose,
    ).run()


if __name__ == "__main__":
    sys.exit(main())
