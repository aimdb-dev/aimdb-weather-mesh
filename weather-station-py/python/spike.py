#!/usr/bin/env python3
"""Exercise the pyo3 door against a real broker.

Design 008 §5.3 claims `StationHandle` is bindable as it stands, that blocking
calls can release the GIL, that the startup gate holds for a foreign caller,
and that `StationError` maps onto something actionable. This checks all four
before the crates are published. Run it with `make spike`; it needs mosquitto
and a debug build of `weather-station-py`.
"""

from __future__ import annotations

import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
MODULE = REPO / "target" / "debug" / "libweather_station.so"

failures: list[str] = []


def check(name: str, condition: bool, detail: str = "") -> None:
    print(f"  {'ok  ' if condition else 'FAIL'}  {name}{f' — {detail}' if detail else ''}")
    if not condition:
        failures.append(name)


def note(name: str, observed: bool, detail: str = "") -> None:
    """Report accepted behaviour the spike measures rather than asserts.

    Not a failure. If one stops reproducing, the crate changed and README.md
    needs to say so.
    """
    print(f"  {'note' if observed else 'gone'}  {name}{f' — {detail}' if detail else ''}")
    if not observed:
        print("        ^ this no longer reproduces — update README.md")


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def load_module(workdir: Path):
    """Import the cdylib the way an installed wheel would be imported."""
    target = workdir / "weather_station.so"
    shutil.copy(MODULE, target)
    sys.path.insert(0, str(workdir))
    import weather_station  # noqa: E402

    return weather_station


def profile(path: Path, *, port: int, slot: str = "slot-17", version: int = 1) -> Path:
    path.write_text(
        f"""
profile_version = {version}
station_id = "{slot}"

[broker]
url = "mqtt://127.0.0.1:{port}"
username = "station-17"
password = "secret"

[app]
name = "spike-station"
lat = 47.07
lon = 15.44
"""
    )
    return path


def main() -> int:
    if not MODULE.exists():
        print(f"no module at {MODULE} — run `cargo build -p weather-station-py` first")
        return 1

    workdir = Path(tempfile.mkdtemp(prefix="station-spike-"))
    ws = load_module(workdir)

    print("\nmodule surface")
    check("Station is exported", hasattr(ws, "Station"))
    check("the error classes are exported", all(hasattr(ws, e) for e in ("StationError", "ProfileError", "BrokerError")))
    check("PROFILE_VERSION crosses the boundary", ws.PROFILE_VERSION == 1, f"= {ws.PROFILE_VERSION}")
    check(
        "the specific errors subclass the general one",
        issubclass(ws.ProfileError, ws.StationError) and issubclass(ws.BrokerError, ws.StationError),
    )

    print("\nprofile failures reach Python as ProfileError")
    port = free_port()
    for name, kwargs in (
        ("a missing profile", {"path": workdir / "absent.toml", "make": False}),
        ("a malformed station_id", {"slot": "station-17"}),
        ("an unsupported profile_version", {"version": 99}),
    ):
        path = kwargs.pop("path", workdir / f"{name.replace(' ', '-')}.toml")
        if kwargs.pop("make", True):
            profile(path, port=port, **kwargs)
        try:
            ws.Station.open_profile(str(path))
            check(name, False, "no exception raised")
        except ws.ProfileError as exc:
            check(name, True, str(exc).splitlines()[0][:60])
        except BaseException as exc:  # noqa: BLE001
            check(name, False, f"{type(exc).__name__}: {exc}")

    print("\nbroker failures reach Python as BrokerError")
    dead = profile(workdir / "dead.toml", port=free_port())
    try:
        ws.Station.open_profile(str(dead))
        check("an unreachable broker", False, "no exception raised")
    except ws.BrokerError as exc:
        check("an unreachable broker", True, str(exc).splitlines()[0][:60])
    except BaseException as exc:  # noqa: BLE001
        check("an unreachable broker", False, f"{type(exc).__name__}: {exc}")

    print("\nthe GIL is released while a join blocks")
    # A listener that accepts and never answers: the pre-flight CONNECT waits
    # its full 15s timeout, which is the longest a station blocks anywhere.
    blackhole = socket.socket()
    blackhole.bind(("127.0.0.1", 0))
    blackhole.listen(8)
    stalled = profile(workdir / "stalled.toml", port=blackhole.getsockname()[1])

    ticks = 0
    stop = threading.Event()

    def ticker() -> None:
        nonlocal ticks
        while not stop.is_set():
            ticks += 1
            time.sleep(0.01)

    thread = threading.Thread(target=ticker, daemon=True)
    thread.start()
    started = time.monotonic()
    try:
        ws.Station.open_profile(str(stalled))
    except BaseException:  # noqa: BLE001
        pass
    blocked_for = time.monotonic() - started
    stop.set()
    thread.join(timeout=1)
    blackhole.close()
    check(
        "a Python thread keeps running while the join blocks",
        ticks > 50,
        f"{ticks} ticks over {blocked_for:.1f}s of blocking",
    )

    print("\nagainst a live broker")
    port = free_port()
    conf = workdir / "mosquitto.conf"
    conf.write_text(f"listener {port} 127.0.0.1\nallow_anonymous true\n")
    broker = subprocess.Popen(
        ["mosquitto", "-c", str(conf)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    time.sleep(0.5)

    # `stdbuf -oL` and a file rather than a pipe: mosquitto_sub block-buffers
    # when its stdout is not a terminal, and the buffer dies with the process.
    captured = workdir / "captured.txt"
    sink = captured.open("w")
    sub = subprocess.Popen(
        ["stdbuf", "-oL", "mosquitto_sub", "-h", "127.0.0.1", "-p", str(port), "-t", "station/#", "-v"],
        stdout=sink,
        text=True,
    )
    time.sleep(0.5)

    live = profile(workdir / "station.toml", port=port)
    runs = 8

    def publish_round(marker: float, *, grace: float) -> tuple[int, int]:
        """Join, publish one of each, close after `grace` seconds. Count arrivals."""
        before = len(captured.read_text().splitlines())
        for i in range(runs):
            station = ws.Station.open_profile(str(live))
            station.publish_temperature(marker + i)
            station.publish_humidity(50.0 + i)
            if grace:
                time.sleep(grace)
            station.close()
        time.sleep(1.0)
        arrived = captured.read_text().splitlines()[before:]
        return (
            len([ln for ln in arrived if "temperature" in ln]),
            len([ln for ln in arrived if "humidity" in ln]),
        )

    try:
        # The supported shape: a station that stays up. Publishing on the line
        # after the join returns is the startup race §5.3's graph-start gate
        # closes, and this is what proves it closed for a foreign caller.
        temps, humid = publish_round(20.0, grace=0.02)
        check(
            "every first reading reaches the broker",
            temps == runs and humid == runs,
            f"{temps}/{runs} temperature, {humid}/{runs} humidity",
        )

        # The other end is deliberately not covered: `publish` returns once the
        # reading is buffered, not once it is on the wire. Accepted, because a
        # station that publishes on a cadence loses only a value nobody reads.
        temps, humid = publish_round(120.0, grace=0.0)
        note(
            "a reading published immediately before close is lost",
            temps < runs or humid < runs,
            f"{temps}/{runs} temperature, {humid}/{runs} humidity arrived",
        )

        lines = captured.read_text().splitlines()
        temps_lines = [ln for ln in lines if "temperature" in ln]
        check(
            "the payload is the versioned contract shape",
            bool(temps_lines)
            and '"schema_version":2' in temps_lines[0]
            and '"celsius"' in temps_lines[0],
            temps_lines[0][:88] if temps_lines else "no payload",
        )
        check(
            "the topic is the mesh's naming rule",
            bool(temps_lines) and temps_lines[0].startswith("station/17/temperature"),
            temps_lines[0].split(" ")[0] if temps_lines else "",
        )

        print("\ntwo stations in one interpreter")
        second = profile(workdir / "station-18.toml", port=port, slot="slot-18")
        a = ws.Station.open_profile(str(live))
        b = ws.Station.open_profile(str(second))
        check("both hold their own slot", (a.slot, b.slot) == (17, 18), f"{a.slot} and {b.slot}")

        # A worker thread publishing through a handle the main thread opened is
        # what `Sync` buys, and it is the shape a real Python station takes:
        # a reader thread per sensor, one seat in the mesh.
        errors: list[BaseException] = []

        def publish_from_thread() -> None:
            try:
                a.publish_temperature(30.5)
                b.publish_temperature(31.5)
            except BaseException as exc:  # noqa: BLE001
                errors.append(exc)

        worker = threading.Thread(target=publish_from_thread)
        worker.start()
        worker.join(timeout=10)
        check("a worker thread can publish through a shared handle", not errors, str(errors[:1]))

        time.sleep(1.0)
        a.close()
        b.close()
        lines = captured.read_text().splitlines()
        check(
            "both slots reach the broker",
            any(ln.startswith("station/17/temperature") and "30.5" in ln for ln in lines)
            and any(ln.startswith("station/18/temperature") and "31.5" in ln for ln in lines),
        )

        print("\nlifecycle")
        station = ws.Station.open_profile(str(live))
        check(
            "slot and name cross the boundary",
            station.slot == 17 and station.name == "spike-station",
            f"slot={station.slot} name={station.name!r}",
        )
        station.close()
        station.close()
        check("close is idempotent", True)
        try:
            station.publish_temperature(1.0)
            check("publishing after close is refused", False, "no exception raised")
        except RuntimeError as exc:
            check("publishing after close is refused", True, str(exc))
        check("a closed station says so", repr(station) == "<Station closed>", repr(station))

        with ws.Station.open_profile(str(live)) as ctx:
            ctx.publish_temperature(21.5)
        check("the context manager closes the station", repr(ctx) == "<Station closed>", repr(ctx))
    finally:
        sub.terminate()
        broker.terminate()
        broker.wait(timeout=5)
        sink.close()

    print()
    if failures:
        print(f"{len(failures)} failing: {', '.join(failures)}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
