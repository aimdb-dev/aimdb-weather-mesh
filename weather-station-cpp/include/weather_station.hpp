// The C++ door: RAII and exceptions over the C ABI in `weather_station.h`.
//
// Header-only, and that is the finding rather than a preference. Rust cannot
// export C++: no class, no std::string, no std::function survives the boundary,
// because those have no stable ABI even between two builds of the same
// compiler. So the shared library exports C, and everything a C++ caller
// actually wants — a destructor that closes, an exception hierarchy, a
// std::filesystem::path overload — is written here, compiled by the consumer's
// own toolchain, and therefore always ABI-compatible with the consumer.
//
// The pendant of `weather-station-py`'s "the ergonomic shape belongs in a
// Python layer over the module". Same conclusion, reached from the other end.

#ifndef WEATHER_STATION_HPP
#define WEATHER_STATION_HPP

#include "weather_station.h"

#include <cstdint>
#include <exception>
#include <filesystem>
#include <stdexcept>
#include <string>
#include <utility>

namespace weather_station {

// The exception hierarchy mirrors the Python module's, and for the same
// reason: a caller catches what it can act on. StationError is the base
// because a caller that cannot distinguish still wants one catch.
class StationError : public std::runtime_error {
public:
    StationError(int status, std::string message)
        : std::runtime_error(std::move(message)), status_(status) {}
    int status() const noexcept { return status_; }

private:
    int status_;
};

// The profile is wrong. Edit it, or have it re-issued.
class ProfileError : public StationError {
public:
    using StationError::StationError;
};

// The broker is unreachable, or refused the credential.
class BrokerError : public StationError {
public:
    using StationError::StationError;
};

// The station has been closed.
class ClosedError : public StationError {
public:
    using StationError::StationError;
};

namespace detail {

inline std::string last_error(int status) {
    const char *message = ws_last_error();
    if (message != nullptr) {
        return std::string(message);
    }
    return "station call failed with status " + std::to_string(status);
}

// Turn a status code into the exception a caller can act on.
//
// The default arm is not optional: ws_status is a C enum, so a library built
// from a later tag can return a code this header has never heard of. Falling
// back to the base class keeps such a code catchable instead of silently
// becoming success.
[[noreturn]] inline void raise(int status) {
    std::string message = last_error(status);
    switch (status) {
    case WS_ERR_PROFILE:
        throw ProfileError(status, std::move(message));
    case WS_ERR_BROKER:
        throw BrokerError(status, std::move(message));
    case WS_ERR_CLOSED:
        throw ClosedError(status, std::move(message));
    default:
        throw StationError(status, std::move(message));
    }
}

inline void check(int status) {
    if (status != WS_OK) {
        raise(status);
    }
}

} // namespace detail

// A station's seat in the mesh: join, publish, close.
//
//     weather_station::Station station("station.toml");
//     station.publish_temperature(21.5f);
//
// Move-only. Copying would give two owners one pointer and two destructors one
// free — the ownership rule the C ABI states in prose, made a compile error.
class Station {
public:
    // Join the mesh from a station.toml path. Blocks; throws on failure.
    //
    // Takes std::filesystem::path rather than const char*, which is what a C++
    // caller reaches for — the pendant of the Python door taking PathBuf so a
    // pathlib.Path works.
    explicit Station(const std::filesystem::path &profile) {
        // .string() rather than .c_str(): on Windows native() is wchar_t, and
        // the ABI takes UTF-8 bytes. See README.md — this conversion is where
        // a non-UTF-8 filesystem path is lost, and the C ABI has no other way
        // to hear about it.
        const std::string path = profile.string();
        detail::check(ws_station_open_profile(path.c_str(), &handle_));
    }

    ~Station() {
        // Never throws: ws_station_free swallows what it must, and a
        // destructor that throws during unwinding calls std::terminate.
        ws_station_free(handle_);
    }

    Station(Station &&other) noexcept : handle_(other.handle_) { other.handle_ = nullptr; }

    Station &operator=(Station &&other) noexcept {
        if (this != &other) {
            ws_station_free(handle_);
            handle_ = other.handle_;
            other.handle_ = nullptr;
        }
        return *this;
    }

    Station(const Station &) = delete;
    Station &operator=(const Station &) = delete;

    // Publish a reading, waiting for room in the slot's buffer.
    //
    // const, and that is the contract rather than an oversight: several sensor
    // threads publish through one station at once. It is the C++ spelling of
    // StationHandle::publish_temperature taking &self.
    void publish_temperature(float celsius) const {
        detail::check(ws_station_publish_temperature(handle_, celsius));
    }

    void publish_humidity(float percent) const {
        detail::check(ws_station_publish_humidity(handle_, percent));
    }

    // The same, failing rather than waiting.
    void try_publish_temperature(float celsius) const {
        detail::check(ws_station_try_publish_temperature(handle_, celsius));
    }

    void try_publish_humidity(float percent) const {
        detail::check(ws_station_try_publish_humidity(handle_, percent));
    }

    // Stop the station. Idempotent, and safe while other threads publish.
    //
    // const for the same reason publish is: a signal handler closing a station
    // its sensor threads are using does not have exclusive access to it.
    void close() const { detail::check(ws_station_close(handle_)); }

    std::uint16_t slot() const noexcept { return ws_station_slot(handle_); }

    // Copied out rather than borrowed. The C ABI's pointer is valid until the
    // station is freed, but a std::string that outlives its Station is the
    // easier thing for a caller to write by accident.
    std::string name() const {
        const char *name = ws_station_name(handle_);
        return name == nullptr ? std::string() : std::string(name);
    }

    bool closed() const noexcept { return ws_station_is_closed(handle_); }

private:
    ws_station *handle_ = nullptr;
};

// Route this library's reporting — and aimdb's — to `sink`.
//
// Returns true if this call installed it, false if a sink was already in
// place. The trampoline is noexcept: it catches everything, because an
// exception leaving it would unwind through Rust frames, which is undefined
// behaviour rather than a crash you can debug.
//
// `sink` must outlive the process; see the header. Taking a raw function
// pointer rather than a std::function is deliberate — a std::function would
// have to be leaked to satisfy that lifetime, and leaking it quietly on the
// caller's behalf is worse than saying so in the signature.
using LogSink = void (*)(int level, const char *target, const char *message, void *user_data);

namespace detail {

// The pair the C ABI cannot carry in one `void *`. Allocated per install and
// handed to Rust as `user_data`, which is what design 050 made possible: the
// sink below the boundary now holds a context pointer, so this header keeps no
// static of its own.
//
// It used to. A function-local `SinkHolder` was assigned on every
// `init_logging` call — written from the caller's thread while aimdb's runtime
// thread read it from the trampoline (a data race), and written *before* asking
// whether the install would be accepted, so a second caller replaced the first
// caller's sink and then returned false to say it had not. Both defects were
// that static; there is no longer one to have them.
struct SinkPair {
    LogSink sink;
    void *user_data;
};

extern "C" inline void sink_trampoline(int level, const char *target, const char *message,
                                       void *pair) noexcept {
    const SinkPair *held = static_cast<const SinkPair *>(pair);
    if (held == nullptr || held->sink == nullptr) {
        return;
    }
    try {
        held->sink(level, target, message, held->user_data);
    } catch (...) {
        // Swallowed on purpose. The alternative is undefined behaviour, and
        // there is nowhere to report to: this is the logging path.
    }
}

} // namespace detail

inline bool init_logging(LogSink sink, void *user_data = nullptr, const char *filter = nullptr) {
    if (sink == nullptr) {
        return false;
    }
    // Owned by the process on the accepted path, exactly as the C contract
    // demands of user_data. On the refused path nothing below kept a pointer to
    // it, so it is this call's to release — which is what makes the false
    // honest: the losing caller changes nothing.
    detail::SinkPair *pair = new detail::SinkPair{sink, user_data};
    if (ws_init_logging(filter, &detail::sink_trampoline, pair)) {
        return true;
    }
    delete pair;
    return false;
}

} // namespace weather_station

#endif // WEATHER_STATION_HPP
