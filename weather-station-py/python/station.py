#!/usr/bin/env python3
"""Weather mesh station in Python, fed by Open-Meteo.

The pendant of `weather-station-openmeteo`: same data source, same profile, a
station that needs no hardware. What differs is where the loop lives. The Rust
template hands aimdb two async sources and lets the record graph drive them;
this one owns its loop and calls `publish_*` when it has a reading, which is
what the blocking door exists for.

    python3 station.py --config station.toml

Publishes into the slot the profile names. The hub derives dew point from the
two records, so nothing here publishes it.

Everything the mesh defines — profile format, slot identity, broker handshake,
record keys and topics — is below the boundary, in `weather_station`. What is
left here is the part a station of your own would replace: where the readings
come from.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import signal
import sys
import threading
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

import weather_station

try:  # 3.11+
    import tomllib
except ModuleNotFoundError:  # 3.9, 3.10 — the door is abi3-py39
    try:
        import tomli as tomllib
    except ModuleNotFoundError:
        raise SystemExit("this station needs Python 3.11+, or `pip install tomli`")

LOG = logging.getLogger("station")

# Open-Meteo refreshes roughly every 15 minutes; polling every 5 keeps the slot
# current without hammering a free API.
POLL_INTERVAL_SECS = 300
HTTP_TIMEOUT_SECS = 10

# Overridable for a self-hosted Open-Meteo, and for testing against a fake.
API_URL = os.environ.get("OPEN_METEO_URL", "https://api.open-meteo.com/v1/forecast")

# Vienna — when neither the profile nor the environment names a location.
DEFAULT_LAT = 48.2082
DEFAULT_LON = 16.3738


def env_coord(var: str) -> float | None:
    """One coordinate from the environment, rejecting a value that is set but
    unparseable rather than falling back to a different location."""
    raw = os.environ.get(var)
    if raw is None:
        return None
    try:
        return float(raw)
    except ValueError:
        raise SystemExit(f"{var}={raw!r} is not a number")


def resolve_location(
    profile: tuple[float | None, float | None],
    env: tuple[float | None, float | None],
) -> tuple[float, float, str]:
    """Pick the coordinates to fetch for: profile, then environment, then Vienna.

    The profile wins when it carries them — a joined station reports from the
    location the mesh published for it, so an environment variable cannot move
    it on the public map. Coordinates are taken as a pair; half a location is an
    error rather than a silent mix of two sources.
    """
    lat, lon = profile
    if lat is not None and lon is not None:
        return float(lat), float(lon), "from station.toml"
    if (lat is None) != (lon is None):
        raise SystemExit(
            "station.toml sets only one of app.lat / app.lon — give both or neither"
        )

    lat, lon = env
    if lat is not None and lon is not None:
        return float(lat), float(lon), "from WEATHER_LAT/WEATHER_LON"
    if (lat is None) != (lon is None):
        raise SystemExit("set WEATHER_LAT and WEATHER_LON together, or neither")

    return DEFAULT_LAT, DEFAULT_LON, "default: Vienna"


def profile_location(config: Path) -> tuple[float | None, float | None]:
    """The station's own two fields out of the profile.

    The mesh tables in the same file are read below the boundary, by
    `Station.open_profile`. This reads only what this station needs, so the two
    never disagree about the parts that matter.
    """
    with open(config, "rb") as f:
        app = tomllib.load(f).get("app", {})
    return app.get("lat"), app.get("lon")


def fetch(lat: float, lon: float) -> tuple[float, float]:
    """One observation: degrees Celsius and percent relative humidity.

    Both come from one request, so they describe the same moment — which is
    what the hub's dew-point join over the two records wants. A sensor such as
    a BME280 behaves the same way: one transaction, both values.
    """
    query = urllib.parse.urlencode(
        {
            "latitude": lat,
            "longitude": lon,
            "current": "temperature_2m,relative_humidity_2m",
        }
    )
    with urllib.request.urlopen(f"{API_URL}?{query}", timeout=HTTP_TIMEOUT_SECS) as response:
        current = json.load(response)["current"]
    return float(current["temperature_2m"]), float(current["relative_humidity_2m"])


def stop_on_signals() -> threading.Event:
    """An event the loop waits on, set by SIGINT and SIGTERM.

    The handler sets a flag and nothing else. Closing the station from inside
    it would run aimdb's shutdown on the signal stack; `close` is safe from
    another thread, which is not the same as safe from a handler.
    """
    stop = threading.Event()
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda *_: stop.set())
    return stop


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--config", type=Path, required=True, help="path to station.toml")
    parser.add_argument("--interval", type=int, default=POLL_INTERVAL_SECS, help="seconds")
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="%(levelname)-5s %(name)s: %(message)s")
    # aimdb's own events, routed into this program's logging rather than
    # written behind its back. Levels stay Python's from here on:
    # logging.getLogger("aimdb_core").setLevel(logging.WARNING)
    weather_station.init_logging()

    lat, lon, source = resolve_location(
        profile_location(args.config),
        (env_coord("WEATHER_LAT"), env_coord("WEATHER_LON")),
    )

    stop = stop_on_signals()

    with weather_station.Station.open_profile(args.config) as station:
        LOG.info("joined slot %d as %r", station.slot, station.name)
        LOG.info("location %.2f, %.2f (%s)", lat, lon, source)

        while not stop.is_set():
            try:
                celsius, percent = fetch(lat, lon)
            except (urllib.error.URLError, OSError, ValueError, KeyError) as e:
                LOG.warning("Open-Meteo fetch failed: %s", e)
            else:
                try:
                    station.publish_temperature(celsius)
                    station.publish_humidity(percent)
                except weather_station.StationError as e:
                    # Publishing outlives nothing: a closed station or a lost
                    # runtime is the end of the loop, not a reading to retry.
                    LOG.error("publish failed: %s", e)
                    return 1
                LOG.info("published %.1f°C, %.1f%%", celsius, percent)

            stop.wait(args.interval)

        LOG.info("stopping")

    # `with` closed the station: the runtime thread is down and the slot is
    # free. A reading published just before this is not guaranteed to have
    # reached the broker — see README.md on what close does not do.
    return 0


if __name__ == "__main__":
    sys.exit(main())
