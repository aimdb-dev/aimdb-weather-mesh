#!/usr/bin/env python3
"""Exercise the pyo3 door against a real broker.

Design 008 §5.3 claims `StationHandle` is bindable as it stands, that blocking
calls can release the GIL, that the startup gate holds for a foreign caller,
and that `StationError` maps onto something actionable. This checks all four
before the crates are published. Run it with `make spike`; it needs mosquitto
and a debug build of `weather-station-py`.

Two rounds are here because the first version of this script missed what they
cover. `Sync` was only ever exercised as a *handover* — a worker publishing
while main waited in `join()` — which is what `Send` buys; two threads inside
one station at once never happened, and that is where `close()` broke. And the
GIL round measured liveness (a Python thread keeps ticking while Rust blocks)
but never deadlock-freedom, which is the failure the `logging` bridge makes
possible. See "shutdown while sensor threads publish" and "lock ordering".
"""

from __future__ import annotations

import logging
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]

#: Tools the live-broker rounds shell out to.
REQUIRED_TOOLS = ("mosquitto", "mosquitto_sub", "stdbuf")

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


def module_path() -> Path | None:
    """Locate the built cdylib.

    Neither half of the path is fixed: `CARGO_TARGET_DIR` moves the target
    directory, and cargo names the artifact `.so` on Linux but `.dylib` on
    macOS. The *import* name is `.so` either way — that is what CPython looks
    for on both — which is why `load_module` copies rather than symlinks.
    """
    target = Path(os.environ.get("CARGO_TARGET_DIR") or REPO / "target") / "debug"
    for name in ("libweather_station.so", "libweather_station.dylib"):
        if (target / name).exists():
            return target / name
    return None


def load_module(built: Path, workdir: Path):
    """Import the cdylib the way an installed wheel would be imported."""
    target = workdir / "weather_station.so"
    shutil.copy(built, target)
    sys.path.insert(0, str(workdir))
    import weather_station  # noqa: E402

    return weather_station


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


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


class Captured(logging.Handler):
    """Collects what the bridge forwards, and whether Python owned the thread.

    The second half is the point. CPython represents a thread it did not itself
    create as a *dummy* thread, named `Dummy-N`, and that is precisely what
    aimdb's runtime thread is once the bridge makes it call into `logging`. A
    record from one is the proof that Rust reached back across the boundary —
    which is the behaviour the bridge adds, and the reason lock ordering starts
    to matter at all.
    """

    def __init__(self) -> None:
        super().__init__()
        self.records: list[tuple[str, str, str, bool]] = []

    def emit(self, record: logging.LogRecord) -> None:
        current = threading.current_thread()
        foreign = type(current).__name__ == "_DummyThread" or current.name.startswith("Dummy-")
        self.records.append(
            (record.name, record.levelname, record.threadName or "?", foreign)
        )

    def foreign_threads(self, records=None) -> set[str]:
        """Names of the emitting threads Python did not create."""
        return {r[2] for r in (self.records if records is None else records) if r[3]}

    def names(self, prefix: str) -> set[str]:
        return {n for n, _, _, _ in self.records if n == prefix or n.startswith(prefix + ".")}

    def at_level(self, prefix: str, level: str) -> int:
        return len(
            [
                r
                for r in self.records
                if (r[0] == prefix or r[0].startswith(prefix + ".")) and r[1] == level
            ]
        )


def under_watchdog(name: str, call, seconds: float = 15.0) -> None:
    """Fail `name` if `call` does not return — a deadlock, not a slow call.

    The GIL round below measures liveness: Rust blocks, Python keeps running.
    This measures the other direction. With the bridge installed, aimdb's
    runtime thread needs the GIL to log, so any call that waits on that thread
    while holding the GIL wedges both. A hang is the symptom, so a plain call
    would hang the whole script; this turns it into one FAIL line.
    """
    outcome: list[BaseException | None] = []

    def run() -> None:
        try:
            call()
            outcome.append(None)
        except BaseException as exc:  # noqa: BLE001
            outcome.append(exc)

    worker = threading.Thread(target=run, daemon=True)
    started = time.monotonic()
    worker.start()
    worker.join(timeout=seconds)
    elapsed = time.monotonic() - started

    if worker.is_alive():
        check(name, False, f"no return within {seconds:.0f}s — deadlocked")
    elif outcome[0] is not None:
        check(name, False, f"{type(outcome[0]).__name__}: {outcome[0]}")
    else:
        check(name, True, f"{elapsed * 1000:.0f} ms")


def module_surface(ws) -> None:
    print("\nmodule surface")
    check("Station is exported", hasattr(ws, "Station"))
    check(
        "the error classes are exported",
        all(hasattr(ws, e) for e in ("StationError", "ProfileError", "BrokerError")),
    )
    # The class name has to match the attribute, or a traceback names something
    # that cannot be imported. `create_exception!` takes it from the Rust
    # identifier, so this is a real thing to get wrong.
    check(
        "the exception classes are named after their attributes",
        all(getattr(ws, e).__name__ == e for e in ("StationError", "ProfileError", "BrokerError")),
        ", ".join(f"{e}={getattr(ws, e).__name__}" for e in ("StationError", "ProfileError")),
    )
    # Not `== 1`: hardcoding the value under test makes a real bump to 2 look
    # like a regression. What crosses the boundary is the type and the fact it
    # is set.
    check(
        "PROFILE_VERSION crosses the boundary",
        isinstance(ws.PROFILE_VERSION, int) and ws.PROFILE_VERSION > 0,
        f"= {ws.PROFILE_VERSION}",
    )
    check(
        "the specific errors subclass the general one",
        issubclass(ws.ProfileError, ws.StationError)
        and issubclass(ws.BrokerError, ws.StationError),
    )
    check(
        "the module does not install a global tracing subscriber",
        not hasattr(ws, "init_tracing"),
        "init_tracing is still exported" if hasattr(ws, "init_tracing") else "",
    )


def logging_bridge(ws, captured: Captured) -> None:
    print("\nthe logging bridge")
    # Installed before anything opens a station, so every round below runs with
    # the runtime thread calling into Python.
    check("init_logging installs the bridge", ws.init_logging() is True)
    # The reason this returns a bool rather than raising: `.init()` panics on a
    # second call, and a Rust panic crosses pyo3 as `PanicException`, which is a
    # BaseException — `except Exception` around logging setup does not catch it.
    # Python code sets logging up twice all the time.
    check("calling it twice is a False, not a panic", ws.init_logging() is False)


def profile_and_broker_errors(ws, workdir: Path) -> None:
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
            # A `Path`, not `str(path)`: the door takes `PathBuf`, so it accepts
            # `str`, `pathlib.Path` and anything else that is `os.PathLike`.
            ws.Station.open_profile(path)
            check(name, False, "no exception raised")
        except ws.ProfileError as exc:
            check(name, True, str(exc).splitlines()[0][:60])
        except BaseException as exc:  # noqa: BLE001
            check(name, False, f"{type(exc).__name__}: {exc}")

    print("\nbroker failures reach Python as BrokerError")
    dead = profile(workdir / "dead.toml", port=free_port())
    try:
        ws.Station.open_profile(dead)
        check("an unreachable broker", False, "no exception raised")
    except ws.BrokerError as exc:
        check("an unreachable broker", True, str(exc).splitlines()[0][:60])
    except BaseException as exc:  # noqa: BLE001
        check("an unreachable broker", False, f"{type(exc).__name__}: {exc}")


def gil_release(ws, workdir: Path) -> None:
    print("\nthe GIL is released while a join blocks")
    # A listener that accepts and never answers, so the pre-flight CONNECT gets
    # a TCP connection and then nothing. rumqttc's own network timeout ends it
    # at ~5s — well inside the crate's 15s PREFLIGHT_TIMEOUT, which is the
    # outer bound rather than what this round measures.
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
        ws.Station.open_profile(stalled)
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


def delivery_rounds(ws, live: Path, captured: Path, runs: int) -> None:
    def publish_round(marker: float, *, grace: float) -> tuple[int, int]:
        """Join, publish one of each, close after `grace` seconds. Count arrivals."""
        before = len(captured.read_text().splitlines())
        for i in range(runs):
            station = ws.Station.open_profile(live)
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
    # The count is what README.md and `StationHandle::close` both quote, so it
    # is printed rather than merely asserted.
    temps, humid = publish_round(120.0, grace=0.0)
    note(
        "a reading published immediately before close is lost",
        temps < runs or humid < runs,
        f"{temps}/{runs} temperature, {humid}/{runs} humidity arrived",
    )


def payload_shape(ws, captured: Path) -> None:
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


def two_stations(ws, live: Path, second: Path, captured: Path) -> None:
    print("\ntwo stations in one interpreter")
    a = ws.Station.open_profile(live)
    b = ws.Station.open_profile(second)
    check("both hold their own slot", (a.slot, b.slot) == (17, 18), f"{a.slot} and {b.slot}")

    # A worker thread publishing through a handle the main thread opened.
    # This is a *handover* — main is parked in `join()` — so it exercises
    # `Send`. What `Sync` adds is the round after it.
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


def concurrent_publishing(ws, live: Path) -> None:
    print("\nsensor threads share one seat")
    # What `Sync` actually buys, and the shape the crate docs describe: several
    # reader threads publishing through one handle *at the same time*, not one
    # after another. Both are `&self` calls, so pyo3 lets them share the borrow.
    station = ws.Station.open_profile(live)
    threads, per_thread = 4, 50
    errors: list[BaseException] = []

    def sensor(reading: float) -> None:
        try:
            for _ in range(per_thread):
                station.publish_temperature(reading)
        except BaseException as exc:  # noqa: BLE001
            errors.append(exc)

    workers = [threading.Thread(target=sensor, args=(20.0 + i,)) for i in range(threads)]
    started = time.monotonic()
    for w in workers:
        w.start()
    for w in workers:
        w.join(timeout=30)
    elapsed = time.monotonic() - started
    check(
        "several threads publish through one handle at once",
        not errors and not any(w.is_alive() for w in workers),
        f"{threads * per_thread} publishes on {threads} threads in {elapsed:.2f}s"
        + (f" — {errors[0]!r}" if errors else ""),
    )
    station.close()


def shutdown_under_load(ws, live: Path) -> None:
    print("\nshutdown while sensor threads publish")
    # The round the first version of this script did not have. `close()` used
    # to need an exclusive borrow, which pyo3 refuses while a `&self` publish
    # is parked in `Python::detach` — so this failed with "Already borrowed",
    # and worse, left the station open, because the borrow is checked before
    # the method body runs. It is the shape a SIGINT handler takes.
    station = ws.Station.open_profile(live)
    stop = threading.Event()
    unexpected: list[BaseException] = []

    def sensor() -> None:
        while not stop.is_set():
            try:
                station.publish_temperature(21.5)
            except ws.StationError:
                return  # the station closed under us; that is the point
            except BaseException as exc:  # noqa: BLE001
                unexpected.append(exc)
                return

    sensors = [threading.Thread(target=sensor) for _ in range(4)]
    for s in sensors:
        s.start()
    time.sleep(0.3)

    # Under a watchdog: a `close` that waits for a publish to finish is a
    # different bug from one that refuses, and both look like a hang here.
    under_watchdog("close() succeeds while sensor threads publish", station.close)

    stop.set()
    for s in sensors:
        s.join(timeout=5)
    check(
        "the publishing threads saw nothing but StationError",
        not unexpected,
        f"{type(unexpected[0]).__name__}: {unexpected[0]}" if unexpected else "",
    )
    check("the station reports itself closed", station.closed, repr(station))


def lock_ordering(ws, live: Path, captured_log: Captured) -> None:
    print("\nlock ordering with the bridge installed")
    # With the bridge, aimdb's runtime thread acquires the GIL to log. Every
    # entry point that waits on that thread therefore has to release the GIL
    # first — `close()` most of all, since it joins it. Each call runs under a
    # watchdog so a wedge is a FAIL rather than a hung script.
    before = len(captured_log.records)

    holder: list = []
    under_watchdog("open_profile while the runtime thread logs", lambda: holder.append(
        ws.Station.open_profile(live)
    ))
    if not holder:
        check("the remaining entry points", False, "no station to call them on")
        return
    station = holder[0]

    under_watchdog("publish_temperature", lambda: station.publish_temperature(21.5))
    under_watchdog("try_publish_humidity", lambda: station.try_publish_humidity(55.0))
    under_watchdog("the getters", lambda: (station.slot, station.name, station.closed, repr(station)))
    under_watchdog("close(), which joins the runtime thread", station.close)

    # The measurement that makes the round mean anything: without a record
    # raised from a thread Python never created, nothing above ever crossed
    # back into Python and the ordering was never exercised.
    emitted = captured_log.records[before:]
    foreign = sorted(captured_log.foreign_threads(emitted))
    check(
        "aimdb's own threads reached Python's logging",
        bool(foreign),
        f"{len(emitted)} records, {len([r for r in emitted if r[3]])} from "
        f"{', '.join(foreign) if foreign else 'this script only'}",
    )


def logger_hierarchy(ws, live: Path, captured_log: Captured) -> None:
    print("\nthe logging bridge routes by subsystem")
    check(
        "the station's events arrive under weather_station",
        bool(captured_log.names("weather_station")),
        ", ".join(sorted(captured_log.names("weather_station"))[:3]),
    )
    # `tracing` targets are Rust module paths (`aimdb_core::builder`) and
    # `logging` splits its hierarchy on `.`, so the bridge translates. Without
    # that, `getLogger("aimdb_core")` is not a parent of anything and setting a
    # level on it does nothing at all.
    check(
        "aimdb's events arrive under their own logger",
        bool(captured_log.names("aimdb_core")),
        ", ".join(sorted(captured_log.names("aimdb_core"))[:3]),
    )
    check(
        "the logger names are dotted, so the hierarchy works",
        not any("::" in n for n, _, _, _ in captured_log.records),
    )

    # What the bridge buys over the old hardcoded filter: this is a runtime
    # decision now, made with the tools a Python operator already has.
    logging.getLogger("aimdb_core").setLevel(logging.WARNING)
    before_aimdb = captured_log.at_level("aimdb_core", "INFO")
    before_station = captured_log.at_level("weather_station", "INFO")
    station = ws.Station.open_profile(live)
    station.publish_temperature(21.5)
    time.sleep(0.5)
    station.close()
    check(
        "one subsystem can be silenced without silencing the others",
        captured_log.at_level("aimdb_core", "INFO") == before_aimdb
        and captured_log.at_level("weather_station", "INFO") > before_station,
        f"aimdb INFO +{captured_log.at_level('aimdb_core', 'INFO') - before_aimdb}, "
        f"station INFO +{captured_log.at_level('weather_station', 'INFO') - before_station}",
    )
    logging.getLogger("aimdb_core").setLevel(logging.NOTSET)


def lifecycle(ws, live: Path) -> None:
    print("\nlifecycle")
    station = ws.Station.open_profile(live)
    check(
        "slot and name cross the boundary",
        station.slot == 17 and station.name == "spike-station",
        f"slot={station.slot} name={station.name!r}",
    )

    # A real test rather than `check(..., True)`: the old form asserted a
    # literal, so it could not fail, and a raising second `close()` would have
    # skipped the check entirely instead of reporting it.
    try:
        station.close()
        station.close()
        check("close is idempotent", True)
    except BaseException as exc:  # noqa: BLE001
        check("close is idempotent", False, f"{type(exc).__name__}: {exc}")

    for label, call in (
        ("publishing", lambda: station.publish_temperature(1.0)),
        ("try-publishing", lambda: station.try_publish_humidity(50.0)),
    ):
        try:
            call()
            check(f"{label} after close is refused", False, "no exception raised")
        except ws.StationError as exc:
            check(f"{label} after close is refused", True, str(exc))
        except BaseException as exc:  # noqa: BLE001
            check(f"{label} after close is refused", False, f"{type(exc).__name__}: {exc}")

    # The slot and the name come from the profile, not the runtime, so they
    # keep answering; `closed` is what carries the state.
    check("a closed station says so", station.closed and "closed" in repr(station), repr(station))
    check(
        "a closed station still knows which slot it held",
        station.slot == 17 and station.name == "spike-station",
        f"slot={station.slot} name={station.name!r}",
    )

    with ws.Station.open_profile(live) as ctx:
        ctx.publish_temperature(21.5)
    check("the context manager closes the station", ctx.closed, repr(ctx))


def run(ws, workdir: Path, captured_log: Captured) -> None:
    """Every round. Raising out of here is a failure, not a crash — see `main`."""
    module_surface(ws)
    logging_bridge(ws, captured_log)
    profile_and_broker_errors(ws, workdir)
    gil_release(ws, workdir)

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
        [
            "stdbuf", "-oL", "mosquitto_sub",
            "-h", "127.0.0.1", "-p", str(port), "-t", "station/#", "-v",
        ],
        stdout=sink,
        text=True,
    )
    time.sleep(0.5)

    live = profile(workdir / "station.toml", port=port)
    second = profile(workdir / "station-18.toml", port=port, slot="slot-18")

    try:
        delivery_rounds(ws, live, captured, runs=8)
        payload_shape(ws, captured)
        two_stations(ws, live, second, captured)
        concurrent_publishing(ws, live)
        shutdown_under_load(ws, live)
        lock_ordering(ws, live, captured_log)
        logger_hierarchy(ws, live, captured_log)
        lifecycle(ws, live)
    finally:
        sub.terminate()
        sub.wait(timeout=5)
        broker.terminate()
        broker.wait(timeout=5)
        sink.close()


def main() -> int:
    built = module_path()
    if built is None:
        print("no module built — run `cargo build -p weather-station-py` first")
        return 1

    missing = [t for t in REQUIRED_TOOLS if shutil.which(t) is None]
    if missing:
        print(f"missing on PATH: {', '.join(missing)} — the live-broker rounds need them")
        print("  (Debian/Ubuntu: apt install mosquitto mosquitto-clients)")
        return 1

    workdir = Path(tempfile.mkdtemp(prefix="station-spike-"))
    ws = load_module(built, workdir)

    # Attached before `init_logging`, so it sees everything the bridge forwards.
    captured_log = Captured()
    logging.getLogger().addHandler(captured_log)
    logging.getLogger().setLevel(logging.DEBUG)

    # The output *is* the result of this script, so a surprise must not take
    # the summary with it. Anything unhandled becomes one recorded failure and
    # a traceback, and the counts below still print.
    try:
        run(ws, workdir, captured_log)
    except BaseException as exc:  # noqa: BLE001
        print()
        traceback.print_exc()
        failures.append(f"the run stopped early ({type(exc).__name__})")

    print()
    if failures:
        print(f"{len(failures)} failing: {', '.join(failures)}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
