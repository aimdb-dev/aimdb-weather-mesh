// Weather mesh station in C++, fed by Open-Meteo.
//
// The pendant of `weather-station-openmeteo`: same data source, same profile, a
// station that needs no hardware. What differs is where the loop lives. The
// Rust template hands aimdb two async sources and lets the record graph drive
// them; this one owns its loop and calls publish_* when it has a reading, which
// is what the blocking door exists for.
//
//     ./station --config station.toml
//
// Everything the mesh defines — profile format, slot identity, broker
// handshake, record keys and topics — is below the C ABI, in the library. What
// is left here is the part a station of your own would replace: where the
// readings come from.

#include "weather_station.hpp"

#include <curl/curl.h>
#include <nlohmann/json.hpp>

#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <optional>
#include <string>
#include <thread>

namespace {

// Open-Meteo refreshes roughly every 15 minutes; polling every 5 keeps the slot
// current without hammering a free API.
constexpr int POLL_INTERVAL_SECS = 300;
constexpr long HTTP_TIMEOUT_SECS = 10;

// Vienna — when neither the profile nor the environment names a location.
constexpr double DEFAULT_LAT = 48.2082;
constexpr double DEFAULT_LON = 16.3738;

// Set by the signal handler, read by the loop. `volatile sig_atomic_t` because
// that is the only thing a handler may portably touch: closing the station from
// inside one would run aimdb's shutdown on the signal stack, and `close` being
// safe from another thread is not the same as safe from a handler.
volatile std::sig_atomic_t g_stop = 0;

extern "C" void on_signal(int) { g_stop = 1; }

// aimdb's own events, and this library's, printed as this program prints.
void log_sink(int level, const char *target, const char *message, void *) {
    const char *name = level >= WS_LOG_ERROR  ? "ERROR"
                       : level >= WS_LOG_WARN ? "WARN"
                       : level >= WS_LOG_INFO ? "INFO"
                                              : "DEBUG";
    std::fprintf(stderr, "%-5s %s: %s\n", name, target, message);
}

// The two values `fetch` returns from one Open-Meteo response — the shape a
// single sensor transaction yields too.
struct Observation {
    double celsius;
    double percent;
};

// Redirects libcurl's response body away from stdout.
std::size_t collect(char *data, std::size_t size, std::size_t count, void *into) {
    static_cast<std::string *>(into)->append(data, size * count);
    return size * count;
}

// The reading out of the response's `current` object. Both field names occur
// twice in a response: under `current` with the numbers, and under
// `current_units` with the unit strings "°C" and "%" — so the lookup starts at
// `current` rather than at the root.
std::optional<Observation> parse_observation(const std::string &body) {
    // Non-throwing, so a body that is malformed or shaped unexpectedly ends the
    // poll rather than the process.
    const nlohmann::json response = nlohmann::json::parse(body, nullptr, false);
    const auto current = response.find("current");
    if (current == response.end()) {
        return std::nullopt;
    }

    const auto celsius = current->find("temperature_2m");
    const auto percent = current->find("relative_humidity_2m");
    if (celsius == current->end() || percent == current->end() || !celsius->is_number() ||
        !percent->is_number()) {
        return std::nullopt;
    }
    return Observation{celsius->get<double>(), percent->get<double>()};
}

std::optional<Observation> fetch(CURL *http, double lat, double lon) {
    const char *base = std::getenv("OPEN_METEO_URL");
    char url[512];
    std::snprintf(url, sizeof(url),
                  "%s?latitude=%.4f&longitude=%.4f&current=temperature_2m,relative_humidity_2m",
                  base != nullptr ? base : "https://api.open-meteo.com/v1/forecast", lat, lon);

    std::string body;
    curl_easy_setopt(http, CURLOPT_URL, url);
    curl_easy_setopt(http, CURLOPT_WRITEFUNCTION, collect);
    curl_easy_setopt(http, CURLOPT_WRITEDATA, &body);
    curl_easy_setopt(http, CURLOPT_TIMEOUT, HTTP_TIMEOUT_SECS);

    const CURLcode code = curl_easy_perform(http);
    if (code != CURLE_OK) {
        std::fprintf(stderr, "WARN  station: Open-Meteo fetch failed: %s\n",
                     curl_easy_strerror(code));
        return std::nullopt;
    }

    const std::optional<Observation> reading = parse_observation(body);
    if (!reading) {
        std::fprintf(stderr, "WARN  station: Open-Meteo response had no reading\n");
    }
    return reading;
}

// One coordinate from the environment, refusing a value that is set but
// unparseable rather than falling back to a different location.
std::optional<double> env_coord(const char *var) {
    const char *raw = std::getenv(var);
    if (raw == nullptr || *raw == '\0') {
        return std::nullopt;
    }
    char *end = nullptr;
    const double value = std::strtod(raw, &end);
    if (*end != '\0') {
        std::fprintf(stderr, "ERROR station: %s='%s' is not a number\n", var, raw);
        std::exit(1);
    }
    return value;
}

struct Location {
    double lat;
    double lon;
    const char *source;
};

// Profile, then environment, then Vienna. The profile wins when it carries
// them: a joined station reports from the location the mesh published for it,
// so an environment variable cannot move it on the public map. Coordinates are
// taken as a pair; half a location is an error rather than a silent mix.
Location resolve_location(std::optional<double> profile_lat, std::optional<double> profile_lon,
                          std::optional<double> env_lat, std::optional<double> env_lon) {
    if (profile_lat && profile_lon) {
        return {*profile_lat, *profile_lon, "from station.toml"};
    }
    if (profile_lat.has_value() != profile_lon.has_value()) {
        std::fprintf(stderr,
                     "ERROR station: station.toml sets only one of app.lat / app.lon — "
                     "give both or neither\n");
        std::exit(1);
    }
    if (env_lat && env_lon) {
        return {*env_lat, *env_lon, "from WEATHER_LAT/WEATHER_LON"};
    }
    if (env_lat.has_value() != env_lon.has_value()) {
        std::fprintf(stderr, "ERROR station: set WEATHER_LAT and WEATHER_LON together, "
                             "or neither\n");
        std::exit(1);
    }
    return {DEFAULT_LAT, DEFAULT_LON, "default: Vienna"};
}

// Sleep, but notice a signal. Returns false if one arrived.
bool nap(int seconds) {
    for (int slept = 0; slept < seconds; ++slept) {
        if (g_stop != 0) {
            return false;
        }
        std::this_thread::sleep_for(std::chrono::seconds(1));
    }
    return g_stop == 0;
}

} // namespace

int main(int argc, char **argv) {
    const char *config = nullptr;
    int interval = POLL_INTERVAL_SECS;
    for (int i = 1; i < argc; ++i) {
        if (std::strcmp(argv[i], "--config") == 0 && i + 1 < argc) {
            config = argv[++i];
        } else if (std::strcmp(argv[i], "--interval") == 0 && i + 1 < argc) {
            interval = std::atoi(argv[++i]);
        }
    }
    if (config == nullptr) {
        std::fprintf(stderr, "usage: %s --config station.toml [--interval seconds]\n", argv[0]);
        return 2;
    }

    weather_station::init_logging(log_sink);
    std::signal(SIGINT, on_signal);
    std::signal(SIGTERM, on_signal);

    const std::filesystem::path profile(config);

    curl_global_init(CURL_GLOBAL_DEFAULT);
    CURL *http = curl_easy_init();
    if (http == nullptr) {
        std::fprintf(stderr, "ERROR station: could not initialise libcurl\n");
        return 1;
    }

    int status = 0;
    try {
        // Joins the mesh, or throws: a revoked slot and a malformed profile
        // raise different exceptions.
        const weather_station::Station station(profile);
        std::fprintf(stderr, "INFO  station: joined slot %u as '%s'\n",
                     static_cast<unsigned>(station.slot()), station.name().c_str());

        const Location location = resolve_location(station.lat(), station.lon(),
                                                   env_coord("WEATHER_LAT"),
                                                   env_coord("WEATHER_LON"));
        std::fprintf(stderr, "INFO  station: location %.2f, %.2f (%s)\n", location.lat,
                     location.lon, location.source);

        do {
            if (const std::optional<Observation> reading = fetch(http, location.lat, location.lon)) {
                station.publish_temperature(static_cast<float>(reading->celsius));
                station.publish_humidity(static_cast<float>(reading->percent));
                std::fprintf(stderr, "INFO  station: published %.1f°C, %.1f%%\n",
                             reading->celsius, reading->percent);
            }
        } while (nap(interval));

        std::fprintf(stderr, "INFO  station: stopping\n");

        // Explicit, though ~Station would also close: this way a failure to
        // shut down cleanly is reported rather than swallowed by a destructor.
        // A reading published just before it is not guaranteed to have reached
        // the broker — see README.md on what close does not do.
        station.close();
    } catch (const weather_station::StationError &e) {
        std::fprintf(stderr, "ERROR station: %s\n", e.what());
        status = 1;
    }

    curl_easy_cleanup(http);
    curl_global_cleanup();
    return status;
}
