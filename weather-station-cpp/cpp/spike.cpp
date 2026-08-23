// Exercise the C ABI door against a real broker.
//
// The pendant of `weather-station-py/python/spike.py`, round for round where a
// round still means something, plus the ones only a C ABI can fail: a panic
// unwinding into C++ frames, a hostile argument, a sink that throws, and a
// destructor that runs while another thread publishes.
//
// Run it with `make spike-cpp`; it needs mosquitto and a debug build of
// `weather-station-cpp`.

#include "weather_station.hpp"

#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <functional>
#include <future>
#include <iostream>
#include <mutex>
#include <set>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

namespace fs = std::filesystem;
using namespace std::chrono_literals;

// --- reporting ------------------------------------------------------------

static std::vector<std::string> failures;

static void check(const std::string &name, bool condition, const std::string &detail = "") {
    std::cout << "  " << (condition ? "ok  " : "FAIL") << "  " << name
              << (detail.empty() ? "" : " — " + detail) << std::endl;
    if (!condition) {
        failures.push_back(name);
    }
}

// A known finding, recorded rather than asserted. Not a failure — but if one
// stops reproducing, the crates changed and README.md needs to say so.
static void note(const std::string &name, bool observed, const std::string &detail = "") {
    std::cout << "  " << (observed ? "note" : "gone") << "  " << name
              << (detail.empty() ? "" : " — " + detail) << std::endl;
    if (!observed) {
        std::cout << "        ^ this no longer reproduces — update README.md" << std::endl;
    }
}

// Fail `name` if `call` does not return — a deadlock, not a slow call.
//
// The pendant of spike.py's watchdog, and it exists for the same reason: with a
// sink installed, aimdb's runtime thread calls out into this program, so any
// call that waits on that thread while holding something the sink needs wedges
// both. A plain call would hang the whole spike; this turns it into one FAIL.
static void under_watchdog(const std::string &name, const std::function<void()> &call,
                           std::chrono::seconds timeout = 15s) {
    auto started = std::chrono::steady_clock::now();
    // The thread is detached on timeout rather than joined: a wedged thread
    // cannot be joined, and the point of this helper is to survive that.
    auto promise = std::make_shared<std::promise<std::string>>();
    auto future = promise->get_future();
    std::thread([promise, call]() {
        try {
            call();
            promise->set_value("");
        } catch (const std::exception &exc) {
            promise->set_value(std::string("threw ") + exc.what());
        } catch (...) {
            promise->set_value("threw a non-std exception");
        }
    }).detach();

    if (future.wait_for(timeout) != std::future_status::ready) {
        check(name, false, "no return within " + std::to_string(timeout.count()) + "s — deadlocked");
        return;
    }
    auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
                       std::chrono::steady_clock::now() - started)
                       .count();
    std::string outcome = future.get();
    if (!outcome.empty()) {
        check(name, false, outcome);
    } else {
        check(name, true, std::to_string(elapsed) + " ms");
    }
}

// --- process and network helpers -----------------------------------------

struct Process {
    pid_t pid = -1;

    void stop() {
        if (pid > 0) {
            kill(pid, SIGTERM);
            int status = 0;
            waitpid(pid, &status, 0);
            pid = -1;
        }
    }
};

static Process spawn(const std::vector<std::string> &args, const std::string &stdout_path = "") {
    pid_t pid = fork();
    if (pid == 0) {
        if (!stdout_path.empty()) {
            FILE *out = freopen(stdout_path.c_str(), "w", stdout);
            (void)out;
        } else {
            FILE *devnull = freopen("/dev/null", "w", stdout);
            (void)devnull;
        }
        FILE *err = freopen("/dev/null", "w", stderr);
        (void)err;
        std::vector<char *> argv;
        for (const auto &arg : args) {
            argv.push_back(const_cast<char *>(arg.c_str()));
        }
        argv.push_back(nullptr);
        execvp(argv[0], argv.data());
        _exit(127);
    }
    return Process{pid};
}

static int free_port() {
    int sock = socket(AF_INET, SOCK_STREAM, 0);
    sockaddr_in addr{};
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = inet_addr("127.0.0.1");
    addr.sin_port = 0;
    bind(sock, reinterpret_cast<sockaddr *>(&addr), sizeof(addr));
    socklen_t len = sizeof(addr);
    getsockname(sock, reinterpret_cast<sockaddr *>(&addr), &len);
    int port = ntohs(addr.sin_port);
    close(sock);
    return port;
}

// A listener that accepts and never answers, so a CONNECT gets a TCP
// connection and then nothing.
struct BlackHole {
    int sock = -1;
    int port = 0;

    BlackHole() {
        sock = socket(AF_INET, SOCK_STREAM, 0);
        sockaddr_in addr{};
        addr.sin_family = AF_INET;
        addr.sin_addr.s_addr = inet_addr("127.0.0.1");
        addr.sin_port = 0;
        bind(sock, reinterpret_cast<sockaddr *>(&addr), sizeof(addr));
        listen(sock, 8);
        socklen_t len = sizeof(addr);
        getsockname(sock, reinterpret_cast<sockaddr *>(&addr), &len);
        port = ntohs(addr.sin_port);
    }
    ~BlackHole() {
        if (sock >= 0) {
            close(sock);
        }
    }
};

static fs::path write_profile(const fs::path &path, int port, const std::string &slot = "slot-17",
                              int version = 1) {
    std::ofstream out(path);
    out << "profile_version = " << version << "\n"
        << "station_id = \"" << slot << "\"\n\n"
        << "[broker]\n"
        << "url = \"mqtt://127.0.0.1:" << port << "\"\n"
        << "username = \"station-17\"\n"
        << "password = \"secret\"\n\n"
        << "[app]\n"
        << "name = \"spike-station\"\n"
        << "lat = 47.07\n"
        << "lon = 15.44\n";
    return path;
}

static std::vector<std::string> read_lines(const fs::path &path) {
    std::vector<std::string> lines;
    std::ifstream in(path);
    std::string line;
    while (std::getline(in, line)) {
        lines.push_back(line);
    }
    return lines;
}

// --- the captured sink ----------------------------------------------------

// Collects what the sink forwards, and from which thread.
//
// The thread id is the point, and it is the C pendant of spike.py checking for
// a `Dummy-N` thread name: a record raised from a thread this program never
// created is the proof that Rust reached back across the boundary, which is
// what makes lock ordering matter at all.
struct Captured {
    std::mutex mutex;
    std::vector<std::tuple<std::string, int, std::thread::id>> records;
    std::atomic<bool> reentered_ok{false};
    std::atomic<bool> reentry_attempted{false};
    std::atomic<const ws_station *> reentry_target{nullptr};
    std::atomic<bool> should_throw{false};

    void add(int level, const char *target, const char *message) {
        (void)message;
        {
            std::lock_guard<std::mutex> guard(mutex);
            records.emplace_back(target, level, std::this_thread::get_id());
        }
        // Called back into the library from inside the sink — that is, from
        // aimdb's runtime thread. Under the Python door this was the
        // deadlock-prone direction (a getter that took a lock the shutdown
        // held, while the shutdown waited for the GIL). Here it is a plain
        // reentrancy question, and the answer has to be "the atomics answer".
        const ws_station *station = reentry_target.load();
        if (station != nullptr) {
            reentry_attempted.store(true);
            (void)ws_station_is_closed(station);
            (void)ws_station_slot(station);
            reentered_ok.store(true);
        }
        if (should_throw.load()) {
            // Round 14. Thrown from inside the sink, on whatever thread
            // emitted the event, and it must not reach a Rust frame.
            throw std::runtime_error("a sink that throws");
        }
    }

    std::set<std::string> targets_with(const std::string &prefix) {
        std::lock_guard<std::mutex> guard(mutex);
        std::set<std::string> found;
        for (const auto &record : records) {
            const std::string &target = std::get<0>(record);
            if (target.rfind(prefix, 0) == 0) {
                found.insert(target);
            }
        }
        return found;
    }

    std::set<std::thread::id> foreign_threads(std::size_t from) {
        std::lock_guard<std::mutex> guard(mutex);
        std::set<std::thread::id> found;
        for (std::size_t i = from; i < records.size(); ++i) {
            found.insert(std::get<2>(records[i]));
        }
        return found;
    }

    std::size_t size() {
        std::lock_guard<std::mutex> guard(mutex);
        return records.size();
    }
};

static Captured captured;
static std::thread::id main_thread_id;

static void sink(int level, const char *target, const char *message, void *user_data) {
    (void)user_data;
    captured.add(level, target, message);
}

// --- rounds ---------------------------------------------------------------

static void abi_surface() {
    std::cout << "\nthe ABI surface" << std::endl;
    check("the header and the library agree on the ABI version",
          ws_abi_version() == WS_ABI_VERSION,
          "library " + std::to_string(ws_abi_version()) + ", header " +
              std::to_string(WS_ABI_VERSION));
    // Not `== 1`: hardcoding the value under test makes a real bump to 2 look
    // like a regression. What crosses is the fact that it is set.
    check("profile_version crosses the boundary", ws_profile_version() > 0,
          "= " + std::to_string(ws_profile_version()));
    check("nothing failed yet, so there is no last error", ws_last_error() == nullptr);
}

static void log_sink_round() {
    std::cout << "\nthe log sink" << std::endl;
    // Installed before anything opens a station, so every round below runs
    // with the runtime thread calling out into this program.
    check("init_logging installs the sink", weather_station::init_logging(sink) == true);
    // The reason this returns a bool rather than aborting: `.init()` panics on
    // a second call, and a panic reaching a C++ frame is undefined behaviour —
    // there is no PanicException here to convert it into. A library inside a
    // library sets logging up twice all the time.
    check("calling it twice is a false, not an abort", weather_station::init_logging(sink) == false);
}

static void hostile_arguments(const fs::path &workdir) {
    std::cout << "\narguments C can send and Rust cannot refuse at compile time" << std::endl;
    ws_station *out = nullptr;

    check("a NULL path is an argument error, not a crash",
          ws_station_open_profile(nullptr, &out) == WS_ERR_INVALID_ARGUMENT);
    check("a NULL out pointer is an argument error, not a crash",
          ws_station_open_profile("station.toml", nullptr) == WS_ERR_INVALID_ARGUMENT);

    // A path that is bytes but not UTF-8. Rust's Path is UTF-8 on this
    // platform, so it cannot be represented at all; the boundary has to say so
    // rather than mangle it.
    const char bad_path[] = {'/', 't', 'm', 'p', '/', '\xff', '\xfe', '\0'};
    check("a non-UTF-8 path is refused rather than mangled",
          ws_station_open_profile(bad_path, &out) == WS_ERR_INVALID_ARGUMENT);

    check("a refused open leaves the out pointer NULL", out == nullptr);
    check("the message survives the status code", ws_last_error() != nullptr,
          ws_last_error() != nullptr ? std::string(ws_last_error()).substr(0, 60) : "NULL");

    // Every entry point that takes a station takes NULL too, because C has no
    // way to stop a caller passing one.
    check("publishing through a NULL station is refused",
          ws_station_publish_temperature(nullptr, 21.5f) == WS_ERR_INVALID_ARGUMENT);
    check("closing a NULL station is refused",
          ws_station_close(nullptr) == WS_ERR_INVALID_ARGUMENT);
    check("the getters answer for a NULL station",
          ws_station_slot(nullptr) == 0 && ws_station_name(nullptr) == nullptr &&
              ws_station_is_closed(nullptr));
    // Freeing NULL has to be a no-op so a destructor can call it unguarded.
    ws_station_free(nullptr);
    check("freeing NULL is a no-op", true);

    (void)workdir;
}

#ifdef WS_SPIKE_PROBE
extern "C" int ws_debug_panic(void);
#endif

static void panic_guard() {
    std::cout << "\nnothing unwinds across the boundary" << std::endl;
#ifdef WS_SPIKE_PROBE
    // The claim this round exists to measure: a Rust panic reaching a C++
    // frame is undefined behaviour, so every entry point catches. Built only
    // with `--features spike-probe`, because a shipped library must not export
    // a way to panic on purpose.
    //
    // What this cannot prove from inside: the guard is compiled out by
    // `panic = "abort"`. A consumer that sets it — and C++ shops that ship
    // with `-fno-exceptions` habits often do — turns every one of these into
    // an abort of the whole process, with no diagnostic this layer can add.
    // stderr is redirected around the call, because what the panic writes
    // there is itself a finding — see the round below.
    const std::string stderr_capture = "/tmp/station-spike-cpp-panic-stderr.txt";
    fflush(stderr);
    int saved_stderr = dup(STDERR_FILENO);
    int capture_fd = open(stderr_capture.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0600);
    dup2(capture_fd, STDERR_FILENO);

    int status = ws_debug_panic();

    fflush(stderr);
    dup2(saved_stderr, STDERR_FILENO);
    close(saved_stderr);
    close(capture_fd);

    check("a Rust panic becomes a status code, not undefined behaviour",
          status == WS_ERR_PANIC, "status " + std::to_string(status));
    check("the panic message survives", ws_last_error() != nullptr,
          ws_last_error() != nullptr ? std::string(ws_last_error()).substr(0, 60) : "NULL");
    check("the process is still standing", true);

    // And the finding the Python door made about logging, restated for panics:
    // an extension is a library inside somebody else's application, and fd 2
    // is the application's. A sink is installed and still the panic went round
    // it, because Rust's panic hook is process-global — which means the *only*
    // fix is upstream: the library must not panic. A hook installed here would
    // be the same trespass `init_tracing` was.
    std::ifstream capture(stderr_capture);
    std::stringstream buffer;
    buffer << capture.rdbuf();
    const std::string written = buffer.str();
    note("a panic writes to stderr behind the application's back", !written.empty(),
         std::to_string(written.size()) + " bytes on fd 2, past the installed sink");
#else
    std::cout << "  skip  the panic probe is not built (cargo --features spike-probe)"
              << std::endl;
#endif
}

static void profile_and_broker_errors(const fs::path &workdir) {
    std::cout << "\nprofile failures reach C++ as ProfileError" << std::endl;
    const int port = free_port();

    struct Case {
        std::string name;
        fs::path path;
        bool make;
        std::string slot;
        int version;
    };
    const std::vector<Case> cases = {
        {"a missing profile", workdir / "absent.toml", false, "slot-17", 1},
        {"a malformed station_id", workdir / "malformed-id.toml", true, "station-17", 1},
        {"an unsupported profile_version", workdir / "future.toml", true, "slot-17", 99},
    };

    for (const auto &c : cases) {
        if (c.make) {
            write_profile(c.path, port, c.slot, c.version);
        }
        try {
            // An fs::path, not a string: the C++ door takes what a C++ caller
            // reaches for, the way the Python door takes PathBuf so a
            // pathlib.Path works.
            weather_station::Station station(c.path);
            check(c.name, false, "no exception thrown");
        } catch (const weather_station::ProfileError &exc) {
            check(c.name, true, std::string(exc.what()).substr(0, 60));
        } catch (const std::exception &exc) {
            check(c.name, false, std::string("wrong type: ") + exc.what());
        }
    }

    std::cout << "\nbroker failures reach C++ as BrokerError" << std::endl;
    const fs::path dead = write_profile(workdir / "dead.toml", free_port());
    try {
        weather_station::Station station(dead);
        check("an unreachable broker", false, "no exception thrown");
    } catch (const weather_station::BrokerError &exc) {
        check("an unreachable broker", true, std::string(exc.what()).substr(0, 60));
    } catch (const std::exception &exc) {
        check("an unreachable broker", false, std::string("wrong type: ") + exc.what());
    }
}

static void blocking_does_not_stop_the_process(const fs::path &workdir) {
    std::cout << "\na blocking call parks its own thread and no other" << std::endl;
    // The pendant of spike.py's GIL round. There is no GIL here, so the claim
    // is weaker but not empty: `open_profile` builds a current-thread Tokio
    // runtime of its own for the pre-flight, and a runtime built per call must
    // not serialise against anything process-wide.
    BlackHole blackhole;
    const fs::path stalled = write_profile(workdir / "stalled.toml", blackhole.port);

    std::atomic<int> ticks{0};
    std::atomic<bool> stop{false};
    std::thread ticker([&]() {
        while (!stop.load()) {
            ticks.fetch_add(1);
            std::this_thread::sleep_for(10ms);
        }
    });

    auto started = std::chrono::steady_clock::now();
    try {
        weather_station::Station station(stalled);
    } catch (const std::exception &) {
    }
    auto blocked = std::chrono::duration_cast<std::chrono::milliseconds>(
                       std::chrono::steady_clock::now() - started)
                       .count();
    stop.store(true);
    ticker.join();

    check("another thread keeps running while the join blocks", ticks.load() > 50,
          std::to_string(ticks.load()) + " ticks over " + std::to_string(blocked / 1000.0) +
              "s of blocking");
}

static std::pair<int, int> publish_round(const fs::path &live, const fs::path &captured_file,
                                         float marker, int runs, std::chrono::milliseconds grace) {
    const std::size_t before = read_lines(captured_file).size();
    for (int i = 0; i < runs; ++i) {
        weather_station::Station station(live);
        station.publish_temperature(marker + static_cast<float>(i));
        station.publish_humidity(50.0f + static_cast<float>(i));
        if (grace.count() > 0) {
            std::this_thread::sleep_for(grace);
        }
        station.close();
    }
    std::this_thread::sleep_for(1s);
    auto lines = read_lines(captured_file);
    int temps = 0;
    int humid = 0;
    for (std::size_t i = before; i < lines.size(); ++i) {
        if (lines[i].find("temperature") != std::string::npos) {
            ++temps;
        }
        if (lines[i].find("humidity") != std::string::npos) {
            ++humid;
        }
    }
    return {temps, humid};
}

static void delivery_rounds(const fs::path &live, const fs::path &captured_file, int runs) {
    // The supported shape: a station that stays up. Publishing on the line
    // after the constructor returns is the startup race the graph-start gate
    // closes, and this is what proves it closed for a foreign caller.
    auto [temps, humid] = publish_round(live, captured_file, 20.0f, runs, 20ms);
    check("every first reading reaches the broker", temps == runs && humid == runs,
          std::to_string(temps) + "/" + std::to_string(runs) + " temperature, " +
              std::to_string(humid) + "/" + std::to_string(runs) + " humidity");

    // The other end is deliberately not covered: publish returns once the
    // reading is buffered, not once it is on the wire. Recorded rather than
    // asserted — how much is lost is timing, and on a fast loopback it can be
    // nothing at all.
    auto [temps2, humid2] = publish_round(live, captured_file, 120.0f, runs, 0ms);
    note("a reading published immediately before close can be lost", temps2 < runs || humid2 < runs,
         std::to_string(temps2) + "/" + std::to_string(runs) + " temperature, " +
             std::to_string(humid2) + "/" + std::to_string(runs) + " humidity arrived");
}

static void payload_shape(const fs::path &captured_file) {
    auto lines = read_lines(captured_file);
    std::string first;
    for (const auto &line : lines) {
        if (line.find("temperature") != std::string::npos) {
            first = line;
            break;
        }
    }
    check("the payload is the versioned contract shape",
          !first.empty() && first.find("\"schema_version\":2") != std::string::npos &&
              first.find("\"celsius\"") != std::string::npos,
          first.empty() ? "no payload" : first.substr(0, 88));
    check("the topic is the mesh's naming rule", first.rfind("station/17/temperature", 0) == 0,
          first.empty() ? "" : first.substr(0, first.find(' ')));
}

static void two_stations(const fs::path &live, const fs::path &second,
                         const fs::path &captured_file) {
    std::cout << "\ntwo stations in one process" << std::endl;
    weather_station::Station a(live);
    weather_station::Station b(second);
    check("both hold their own slot", a.slot() == 17 && b.slot() == 18,
          std::to_string(a.slot()) + " and " + std::to_string(b.slot()));

    std::vector<std::string> errors;
    std::thread worker([&]() {
        try {
            a.publish_temperature(30.5f);
            b.publish_temperature(31.5f);
        } catch (const std::exception &exc) {
            errors.push_back(exc.what());
        }
    });
    worker.join();
    check("a worker thread can publish through a station main opened", errors.empty(),
          errors.empty() ? "" : errors[0]);

    std::this_thread::sleep_for(1s);
    a.close();
    b.close();
    auto lines = read_lines(captured_file);
    bool seen_a = false;
    bool seen_b = false;
    for (const auto &line : lines) {
        if (line.rfind("station/17/temperature", 0) == 0 && line.find("30.5") != std::string::npos) {
            seen_a = true;
        }
        if (line.rfind("station/18/temperature", 0) == 0 && line.find("31.5") != std::string::npos) {
            seen_b = true;
        }
    }
    check("both slots reach the broker", seen_a && seen_b);
}

static void concurrent_publishing(const fs::path &live) {
    std::cout << "\nsensor threads share one seat" << std::endl;
    // What `Sync` buys, spelled in C++: several threads publishing through one
    // `const Station&` at the same time. The C ABI takes `const ws_station*`
    // everywhere but free, which is what makes this legal without a mutex.
    weather_station::Station station(live);
    const int threads = 4;
    const int per_thread = 50;
    std::atomic<int> failed{0};
    std::vector<std::thread> workers;
    auto started = std::chrono::steady_clock::now();
    for (int i = 0; i < threads; ++i) {
        workers.emplace_back([&station, &failed, i]() {
            for (int n = 0; n < per_thread; ++n) {
                try {
                    station.publish_temperature(20.0f + static_cast<float>(i));
                } catch (const std::exception &) {
                    failed.fetch_add(1);
                    return;
                }
            }
        });
    }
    for (auto &worker : workers) {
        worker.join();
    }
    auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
                       std::chrono::steady_clock::now() - started)
                       .count();
    check("several threads publish through one station at once", failed.load() == 0,
          std::to_string(threads * per_thread) + " publishes on " + std::to_string(threads) +
              " threads in " + std::to_string(elapsed / 1000.0) + "s");
    station.close();
}

static void fork_round(const fs::path &live, const fs::path &captured_file) {
    std::cout << "\nafter fork(), the child has no runtime thread" << std::endl;
    // No pendant in the Python door, which never asked. It matters more here:
    // a C++ daemon that double-forks, or a supervisor that fork()s per job, is
    // an ordinary shape, and fork copies the address space but not the
    // threads. The station object survives; the thread that pumps its graph
    // does not.
    //
    // What makes this a finding rather than a caveat is what the child is
    // told: `publish` reports success and `is_closed` reports open, and the
    // reading is dropped. That is the failure mode the graph-start gate exists
    // to prevent, reappearing on the other side of a fork.
    weather_station::Station station(live);
    station.publish_temperature(11.0f); // parent, before the fork
    std::this_thread::sleep_for(300ms);

    pid_t pid = fork();
    if (pid == 0) {
        // Nothing here may allocate through a lock another thread held at the
        // moment of the fork, so this is deliberately three calls and an exit.
        bool closed = station.closed();
        int published = WS_ERR_RUNTIME;
        try {
            station.publish_temperature(99.0f);
            published = WS_OK;
        } catch (...) {
        }
        _exit((closed ? 2 : 0) | (published == WS_OK ? 4 : 0));
    }

    int status = 0;
    waitpid(pid, &status, 0);
    const int code = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    const bool child_saw_open = (code & 2) == 0;
    const bool child_publish_succeeded = (code & 4) != 0;

    station.publish_temperature(12.0f); // parent, after the fork
    std::this_thread::sleep_for(1s);
    station.close();

    bool child_reading_arrived = false;
    bool parent_readings_arrived = false;
    int parent_seen = 0;
    for (const auto &line : read_lines(captured_file)) {
        if (line.find("99.0") != std::string::npos) {
            child_reading_arrived = true;
        }
        if (line.find("11.0") != std::string::npos || line.find("12.0") != std::string::npos) {
            ++parent_seen;
        }
    }
    parent_readings_arrived = parent_seen >= 2;

    check("the parent keeps publishing across the fork", parent_readings_arrived,
          std::to_string(parent_seen) + " of the parent's 2 readings arrived");
    note("a forked child is told the station is open", child_saw_open,
         "is_closed() reported " + std::string(child_saw_open ? "open" : "closed"));
    note("a forked child's publish reports success", child_publish_succeeded,
         child_publish_succeeded ? "returned WS_OK" : "refused");
    note("and the reading is dropped in silence", !child_reading_arrived,
         child_reading_arrived ? "the child's 99.0 reached the broker after all"
                               : "the child's 99.0 never reached the broker");
}

static void shutdown_under_load(const fs::path &live) {
    std::cout << "\nshutdown while sensor threads publish" << std::endl;
    // The round the Python door needed a crate fix to pass: `close` used to
    // need exclusive access, and could not get it while a publish was in
    // flight. Here the same property is what lets `close()` be a const method
    // — and it is the shape a SIGINT handler takes.
    weather_station::Station station(live);
    std::atomic<bool> stop{false};
    std::atomic<int> unexpected{0};
    std::vector<std::thread> sensors;
    for (int i = 0; i < 4; ++i) {
        sensors.emplace_back([&]() {
            while (!stop.load()) {
                try {
                    station.publish_temperature(21.5f);
                } catch (const weather_station::StationError &) {
                    return; // the station closed under us; that is the point
                } catch (...) {
                    unexpected.fetch_add(1);
                    return;
                }
            }
        });
    }
    std::this_thread::sleep_for(300ms);

    under_watchdog("close() succeeds while sensor threads publish", [&]() { station.close(); });

    stop.store(true);
    for (auto &sensor : sensors) {
        sensor.join();
    }
    check("the publishing threads saw nothing but StationError", unexpected.load() == 0);
    check("the station reports itself closed", station.closed());
}

static void destructor_under_load(const fs::path &live) {
    std::cout << "\nthe destructor runs while sensor threads publish" << std::endl;
    // No pendant in the Python door: there, the interpreter's reference count
    // decides when the object dies, and a thread still publishing through it
    // is holding a reference. C++ has no such guarantee, so the destructor is
    // a free — the one entry point that is not thread-safe. This round runs
    // the sequence a correct caller writes (stop the threads, then destroy)
    // and checks that the *close* inside the destructor is what ends the
    // publishing, not a crash.
    std::atomic<bool> stop{false};
    std::atomic<int> unexpected{0};
    std::vector<std::thread> sensors;
    {
        weather_station::Station station(live);
        for (int i = 0; i < 4; ++i) {
            sensors.emplace_back([&]() {
                while (!stop.load()) {
                    try {
                        station.publish_temperature(22.5f);
                    } catch (const weather_station::StationError &) {
                        return;
                    } catch (...) {
                        unexpected.fetch_add(1);
                        return;
                    }
                }
            });
        }
        std::this_thread::sleep_for(300ms);
        // Close first, then let the threads notice, then destroy. The ordering
        // is the caller's to get right, and it is the ordering the header
        // documents.
        station.close();
        stop.store(true);
        for (auto &sensor : sensors) {
            sensor.join();
        }
    }
    check("close-then-join-then-destroy is clean", unexpected.load() == 0);

    // A move leaves the source with nothing to free, so a second destructor is
    // harmless. The ownership rule the C ABI can only state in prose, made a
    // property of the type.
    weather_station::Station moved_from(live);
    weather_station::Station moved_to(std::move(moved_from));
    moved_to.close();
    check("a moved-from station destructs without a double free", true,
          "slot " + std::to_string(moved_to.slot()));
}

static void lock_ordering(const fs::path &live) {
    std::cout << "\nlock ordering with the sink installed" << std::endl;
    // With the sink, aimdb's runtime thread calls out into this program. Every
    // entry point that waits on that thread is therefore part of a lock
    // ordering — `close()` most of all, since it joins it. Each call runs
    // under a watchdog so a wedge is a FAIL rather than a hung spike.
    const std::size_t before = captured.size();

    std::shared_ptr<weather_station::Station> station;
    under_watchdog("open_profile while the runtime thread logs",
                   [&]() { station = std::make_shared<weather_station::Station>(live); });
    if (!station) {
        check("the remaining entry points", false, "no station to call them on");
        return;
    }

    under_watchdog("publish_temperature", [&]() { station->publish_temperature(21.5f); });
    under_watchdog("try_publish_humidity", [&]() { station->try_publish_humidity(55.0f); });
    under_watchdog("the getters", [&]() {
        (void)station->slot();
        (void)station->name();
        (void)station->closed();
    });
    under_watchdog("close(), which joins the runtime thread", [&]() { station->close(); });

    // The measurement that makes the round mean anything: without a record
    // raised from a thread this program never created, nothing above ever
    // crossed back and the ordering was never exercised.
    auto threads = captured.foreign_threads(before);
    threads.erase(main_thread_id);
    check("aimdb's own threads reached the sink", !threads.empty(),
          std::to_string(captured.size() - before) + " records from " +
              std::to_string(threads.size()) + " thread(s) this program did not create");
}

static void reentrancy(const fs::path &live) {
    std::cout << "\nthe sink calls back into the library" << std::endl;
    // The direction the Python door had to fix in the crate: a getter called
    // from inside the logging path — that is, on aimdb's runtime thread —
    // while the station it asks about is live and later while it is shutting
    // down. There `is_closed` had to read an atomic rather than the mutex
    // `shutdown` holds, or a caller under the GIL would deadlock against its
    // own shutdown. Here there is no GIL to serialise anything, so if the
    // getters ever start taking that lock this is the round that hangs.
    //
    // The C++ wrapper hides the raw pointer, which is the right shape for a
    // caller and the wrong one for this round; go through the ABI directly.
    ws_station *target = nullptr;
    const std::string path = live.string();
    check("a station opens for the reentrancy round",
          ws_station_open_profile(path.c_str(), &target) == WS_OK && target != nullptr);
    if (target == nullptr) {
        return;
    }

    captured.reentered_ok.store(false);
    captured.reentry_attempted.store(false);
    captured.reentry_target.store(target);

    // A second station, opened and closed purely to make the runtime thread
    // talk. Every event it emits reenters the getters on the first station.
    under_watchdog("a second station's events reenter the first station's getters", [&]() {
        ws_station *chatty = nullptr;
        if (ws_station_open_profile(path.c_str(), &chatty) == WS_OK) {
            (void)ws_station_publish_temperature(chatty, 23.5f);
            (void)ws_station_close(chatty);
            ws_station_free(chatty);
        }
    });
    check("the sink did reenter", captured.reentry_attempted.load());
    check("the reentrant calls returned", captured.reentered_ok.load());

    // And now the sharp case: the getters are reentered from the runtime
    // thread of the very station being shut down.
    under_watchdog("close() while the sink reenters that station's own getters",
                   [&]() { (void)ws_station_close(target); });

    captured.reentry_target.store(nullptr);
    ws_station_free(target);
}

static void throwing_sink(const fs::path &live) {
    std::cout << "\na sink that throws does not unwind into Rust" << std::endl;
    // A C++ exception crossing an extern "C" frame is undefined behaviour, not
    // a crash you can debug. The header's trampoline is `noexcept` and catches
    // everything; this round proves the program survives a sink that throws on
    // every event, which is what a caller's logging library does the first
    // time a disk fills up.
    captured.should_throw.store(true);
    under_watchdog("a station opens and closes with a throwing sink", [&]() {
        weather_station::Station station(live);
        station.publish_temperature(24.5f);
        station.close();
    });
    captured.should_throw.store(false);
    check("the process is still standing", true);
}

static void sink_routing() {
    std::cout << "\nthe sink reports which subsystem spoke" << std::endl;
    check("the station's events arrive under weather_station",
          !captured.targets_with("weather_station").empty(),
          [&]() {
              std::ostringstream out;
              int n = 0;
              for (const auto &target : captured.targets_with("weather_station")) {
                  if (n++ >= 3) {
                      break;
                  }
                  out << (n > 1 ? ", " : "") << target;
              }
              return out.str();
          }());
    check("aimdb's events arrive under their own target",
          !captured.targets_with("aimdb_core").empty(), [&]() {
              std::ostringstream out;
              int n = 0;
              for (const auto &target : captured.targets_with("aimdb_core")) {
                  if (n++ >= 3) {
                      break;
                  }
                  out << (n > 1 ? ", " : "") << target;
              }
              return out.str();
          }());
    // Unlike the Python door, nothing is translated: `logging` splits its
    // hierarchy on `.` and needed `::` rewritten, while C has no hierarchy at
    // all. What a C caller gets is the raw target and a `strncmp`.
    bool dotted = false;
    for (const auto &target : captured.targets_with("aimdb_core")) {
        if (target.find("::") != std::string::npos) {
            dotted = true;
        }
    }
    note("the target reaches C as a Rust module path, separators intact", dotted,
         "a C caller filters with strncmp, not a logger hierarchy");
}

static void lifecycle(const fs::path &live) {
    std::cout << "\nlifecycle" << std::endl;
    weather_station::Station station(live);
    check("slot and name cross the boundary", station.slot() == 17 && station.name() == "spike-station",
          "slot=" + std::to_string(station.slot()) + " name=" + station.name());

    try {
        station.close();
        station.close();
        check("close is idempotent", true);
    } catch (const std::exception &exc) {
        check("close is idempotent", false, exc.what());
    }

    struct Call {
        std::string label;
        std::function<void()> run;
    };
    const std::vector<Call> calls = {
        {"publishing", [&]() { station.publish_temperature(1.0f); }},
        {"try-publishing", [&]() { station.try_publish_humidity(50.0f); }},
    };
    for (const auto &call : calls) {
        try {
            call.run();
            check(call.label + " after close is refused", false, "no exception thrown");
        } catch (const weather_station::ClosedError &exc) {
            check(call.label + " after close is refused", true, exc.what());
        } catch (const std::exception &exc) {
            check(call.label + " after close is refused", false,
                  std::string("wrong type: ") + exc.what());
        }
    }

    check("a closed station says so", station.closed());
    check("a closed station still knows which slot it held",
          station.slot() == 17 && station.name() == "spike-station",
          "slot=" + std::to_string(station.slot()) + " name=" + station.name());

    // The pendant of the `with` block: the scope is the lifetime.
    bool closed_by_scope = false;
    {
        weather_station::Station scoped(live);
        scoped.publish_temperature(21.5f);
        // Observed through the raw ABI after the destructor has run would be a
        // use-after-free; what is checkable is that the destructor returns and
        // the next station can take the slot, which the round after this does.
        closed_by_scope = !scoped.closed();
    }
    check("a scoped station is open until the scope ends", closed_by_scope);
    weather_station::Station after(live);
    check("the slot is free again once the scope ended", after.slot() == 17);
    after.close();
}

int main() {
    main_thread_id = std::this_thread::get_id();

    char temp_dir[] = "/tmp/station-spike-cpp-XXXXXX";
    if (mkdtemp(temp_dir) == nullptr) {
        std::cerr << "cannot create a working directory" << std::endl;
        return 1;
    }
    const fs::path workdir(temp_dir);

    abi_surface();
    log_sink_round();
    hostile_arguments(workdir);
    panic_guard();
    profile_and_broker_errors(workdir);
    blocking_does_not_stop_the_process(workdir);

    std::cout << "\nagainst a live broker" << std::endl;
    const int port = free_port();
    const fs::path conf = workdir / "mosquitto.conf";
    {
        std::ofstream out(conf);
        out << "listener " << port << " 127.0.0.1\nallow_anonymous true\n";
    }
    Process broker = spawn({"mosquitto", "-c", conf.string()});
    std::this_thread::sleep_for(500ms);

    // `stdbuf -oL` and a file rather than a pipe: mosquitto_sub block-buffers
    // when its stdout is not a terminal, and the buffer dies with the process.
    const fs::path captured_file = workdir / "captured.txt";
    Process sub = spawn({"stdbuf", "-oL", "mosquitto_sub", "-h", "127.0.0.1", "-p",
                         std::to_string(port), "-t", "station/#", "-v"},
                        captured_file.string());
    std::this_thread::sleep_for(500ms);

    const fs::path live = write_profile(workdir / "station.toml", port);
    const fs::path second = write_profile(workdir / "station-18.toml", port, "slot-18");

    try {
        delivery_rounds(live, captured_file, 8);
        payload_shape(captured_file);
        two_stations(live, second, captured_file);
        concurrent_publishing(live);
        fork_round(live, captured_file);
        shutdown_under_load(live);
        destructor_under_load(live);
        lock_ordering(live);
        reentrancy(live);
        throwing_sink(live);
        sink_routing();
        lifecycle(live);
    } catch (const std::exception &exc) {
        std::cout << "\nthe run stopped early: " << exc.what() << std::endl;
        failures.push_back(std::string("the run stopped early (") + exc.what() + ")");
    }

    sub.stop();
    broker.stop();

    std::cout << std::endl;
    if (!failures.empty()) {
        std::cout << failures.size() << " failing:";
        for (const auto &failure : failures) {
            std::cout << " " << failure << ";";
        }
        std::cout << std::endl;
        return 1;
    }
    std::cout << "all checks passed" << std::endl;
    return 0;
}
