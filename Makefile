# AimDB Weather Mesh Makefile
#
# Mirrors the aimdb repository's convention: every feature combination CI
# verifies is spelled out here, so a local `make check` and a CI run execute
# the same commands. Adding a feature to a crate means adding its combination
# here — that is the only place the matrix lives.

.PHONY: help build test fmt fmt-check clippy test-embedded lockfile check clean clean-embedded \
	ts-bindings ts-bindings-check wasm-check wasm js station-py station-cpp
.DEFAULT_GOAL := help

# Separate target dir for the cross-compile checks, so an interrupted embedded
# build cannot leave .rmeta files that break the next host `cargo check`.
EMBEDDED_CHECK_TARGET_DIR := target/embedded-check
EMBEDDED_TARGET := thumbv7em-none-eabihf

# The browser client. Its Rust half only compiles for wasm32 (the wasm adapter
# holds `web_sys` closures across await points, so its futures are `!Send`), and
# its published artifact is an npm package rather than a crate.
WASM_TARGET := wasm32-unknown-unknown
WASM_CHECK_TARGET_DIR := target/wasm-check
JS_DIR := weather-mesh-client/js
# ts-rs writes one file per type here. Chosen by the caller rather than baked
# into a `#[ts(export_to = ...)]` attribute, so no contract source hardcodes a
# path into a sibling crate.
TS_BINDINGS_DIR := $(JS_DIR)/src/generated
TS_BINDINGS_FEATURES := ts,linkable,migratable

# Every workspace member. `cargo fmt --all` is deliberately not used: it walks
# into path dependencies outside this repository (the sibling aimdb checkout
# and its embassy submodule), which both formats code we do not own and makes
# the result depend on whether that submodule is present.
PACKAGES := weather-contracts weather-station weather-station-openmeteo weather-station-knx weather-hub weather-mesh-client weather-station-py weather-station-cpp

# Many cargo invocations in sequence with different feature sets can hit
# "Stale file handle" linker errors on Docker overlay filesystems.
export CARGO_INCREMENTAL := 0

GREEN := \033[0;32m
YELLOW := \033[0;33m
BLUE := \033[0;34m
RED := \033[0;31m
NC := \033[0m

## Show available commands
help:
	@printf "$(GREEN)AimDB Weather Mesh Development Commands$(NC)\n"
	@printf "\n"
	@printf "  $(YELLOW)Core Commands:$(NC)\n"
	@printf "    build          Build the workspace\n"
	@printf "    test           Run tests across all valid feature combinations\n"
	@printf "    fmt            Format code\n"
	@printf "    fmt-check      Check formatting (CI mode)\n"
	@printf "    clippy         Lint all valid feature combinations\n"
	@printf "\n"
	@printf "  $(YELLOW)Release-readiness Commands:$(NC)\n"
	@printf "    test-embedded  Cross-compile the no_std crates for $(EMBEDDED_TARGET)\n"
	@printf "    wasm-check     Compile the browser client for $(WASM_TARGET)\n"
	@printf "    ts-bindings    Regenerate the TypeScript contract types (ts-rs)\n"
	@printf "    js             Typecheck and test the browser client facade\n"
	@printf "    lockfile       Fail if Cargo.lock is stale for the current sibling checkout\n"
	@printf "    check          Everything above — run before pushing\n"
	@printf "\n"
	@printf "  $(YELLOW)FFI stations:$(NC)\n"
	@printf "    station-py     Run the Python station (CONFIG=station.local.toml)\n"
	@printf "    station-cpp    Run the C++ station (CONFIG=station.local.toml, needs libcurl + nlohmann)\n"
	@printf "\n"
	@printf "  $(YELLOW)Browser client:$(NC)\n"
	@printf "    wasm           Build the npm package with wasm-pack (needs wasm-pack)\n"
	@printf "\n"
	@printf "  $(YELLOW)Housekeeping:$(NC)\n"
	@printf "    clean          Remove build artifacts\n"
	@printf "\n"
	@printf "  $(BLUE)Note:$(NC) this workspace reaches the aimdb crates by path, so a sibling\n"
	@printf "        aimdb checkout must exist at ../aimdb.\n"

CONFIG ?= station.local.toml

## Run the Python station — see weather-station-py/README.md
##
## The module is copied to `weather_station.so` and reached through PYTHONPATH,
## which is what an installed wheel does for you. Until there is a wheel, this
## is the import path.
station-py:
	@printf "$(GREEN)Building the Python module...$(NC)\n"
	cargo build -p weather-station-py
	@cp $(FFI_TARGET_DIR)/libweather_station.so $(FFI_TARGET_DIR)/weather_station.so
	@printf "$(GREEN)Starting the station ($(CONFIG))...$(NC)\n"
	PYTHONPATH=$(FFI_TARGET_DIR) python3 weather-station-py/python/station.py --config $(CONFIG)

## Run the C++ station — see weather-station-cpp/README.md
##
## Linked against the cdylib the way a consuming build would link it. libcurl and
## nlohmann/json are this station's own dependencies, not the library's: they are
## where the readings come from, which is the half a station of your own replaces.
station-cpp:
	@printf "$(GREEN)Building the C ABI library...$(NC)\n"
	cargo build -p weather-station-cpp
	@printf "$(GREEN)Building the station...$(NC)\n"
	$(CXX) $(CXXFLAGS) -Iweather-station-cpp/include \
		weather-station-cpp/cpp/station.cpp \
		-L$(FFI_TARGET_DIR) -lweather_station_ffi -lcurl \
		-o $(FFI_TARGET_DIR)/station-cpp
	@printf "$(GREEN)Starting the station ($(CONFIG))...$(NC)\n"
	LD_LIBRARY_PATH=$(FFI_TARGET_DIR) $(FFI_TARGET_DIR)/station-cpp --config $(CONFIG)

CXX ?= g++
CXXFLAGS ?= -std=c++17 -Wall -Wextra -g -pthread
FFI_TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)/debug

## Build the workspace
build:
	@printf "$(GREEN)Building workspace...$(NC)\n"
	cargo build --workspace --all-targets

## Run tests across all valid feature combinations
test:
	@printf "$(GREEN)Running tests (all valid combinations)...$(NC)\n"
	@printf "$(YELLOW)  → Testing weather-contracts (std + linkable + simulatable + migratable)$(NC)\n"
	cargo test -p weather-contracts --features "std,linkable,simulatable,migratable"
	@printf "$(YELLOW)  → Testing weather-contracts (no_std + linkable + migratable)$(NC)\n"
	cargo test -p weather-contracts --no-default-features --features "linkable,migratable"
	@printf "$(YELLOW)  → Testing weather-station (tokio-runtime, the template default)$(NC)\n"
	cargo test -p weather-station
	@printf "$(YELLOW)  → Testing weather-station (sync — the blocking door the FFI layers bind)$(NC)\n"
	cargo test -p weather-station --features sync
	@printf "$(YELLOW)  → Testing weather-station (sync + rustls — the FFI shared-library build)$(NC)\n"
	cargo test -p weather-station --no-default-features --features "tokio-runtime,rustls,sync"
	@printf "$(YELLOW)  → Testing weather-station (sync, no TLS backend — mqtt:// only)$(NC)\n"
	cargo test -p weather-station --no-default-features --features "tokio-runtime,sync"
	@printf "$(YELLOW)  → Testing weather-station-openmeteo$(NC)\n"
	cargo test -p weather-station-openmeteo
	@printf "$(YELLOW)  → Testing weather-station-knx$(NC)\n"
	cargo test -p weather-station-knx
	@printf "$(YELLOW)  → Testing weather-hub$(NC)\n"
	cargo test -p weather-hub
	@printf "$(YELLOW)  → Testing weather-mesh-client (the exported key rule)$(NC)\n"
	cargo test -p weather-mesh-client
	@printf "$(YELLOW)  → Testing weather-station-cpp (the log sink's first-wins contract)$(NC)\n"
	cargo test -p weather-station-cpp --lib
	@printf "$(GREEN)✓ All tests passed!$(NC)\n"

## Format code
fmt:
	@printf "$(GREEN)Formatting code...$(NC)\n"
	@for pkg in $(PACKAGES); do \
		printf "$(YELLOW)  → Formatting $$pkg$(NC)\n"; \
		cargo fmt -p $$pkg; \
	done

## Check formatting (CI mode)
fmt-check:
	@printf "$(GREEN)Checking code formatting (workspace members only)...$(NC)\n"
	@FAILED=0; \
	for pkg in $(PACKAGES); do \
		printf "$(YELLOW)  → Checking $$pkg$(NC)\n"; \
		if ! cargo fmt -p $$pkg -- --check 2>&1; then \
			printf "$(RED)❌ Formatting check failed for $$pkg$(NC)\n"; \
			FAILED=1; \
		fi; \
	done; \
	if [ $$FAILED -eq 1 ]; then \
		printf "$(RED)✗ Formatting check failed! Run 'make fmt' to fix.$(NC)\n"; \
		exit 1; \
	fi
	@printf "$(GREEN)✓ All packages properly formatted!$(NC)\n"

## Lint all valid feature combinations
clippy:
	@printf "$(GREEN)Running clippy (all valid combinations)...$(NC)\n"
	@printf "$(YELLOW)  → Clippy on weather-contracts (std + everything)$(NC)\n"
	cargo clippy -p weather-contracts --features "std,linkable,simulatable,migratable" --all-targets -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-contracts (no_std, schemas only)$(NC)\n"
	cargo clippy -p weather-contracts --no-default-features -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-contracts (no_std + linkable + migratable)$(NC)\n"
	cargo clippy -p weather-contracts --no-default-features --features "linkable,migratable" -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-station (tokio-runtime)$(NC)\n"
	cargo clippy -p weather-station --all-targets -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-station (sync)$(NC)\n"
	cargo clippy -p weather-station --features sync --all-targets -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-station (sync + rustls)$(NC)\n"
	cargo clippy -p weather-station --no-default-features --features "tokio-runtime,rustls,sync" --all-targets -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-station (sync, no TLS backend)$(NC)\n"
	cargo clippy -p weather-station --no-default-features --features "tokio-runtime,sync" --all-targets -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-station (no_std, MCU feature set)$(NC)\n"
	cargo clippy -p weather-station --no-default-features -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-station-openmeteo$(NC)\n"
	cargo clippy -p weather-station-openmeteo --all-targets -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-station-knx$(NC)\n"
	cargo clippy -p weather-station-knx --all-targets -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-hub$(NC)\n"
	cargo clippy -p weather-hub --all-targets -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-mesh-client (host: the key rule)$(NC)\n"
	cargo clippy -p weather-mesh-client --all-targets -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-mesh-client ($(WASM_TARGET): the fusion)$(NC)\n"
	cargo clippy -p weather-mesh-client --target $(WASM_TARGET) --target-dir $(WASM_CHECK_TARGET_DIR) -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-station-py (the pyo3 door)$(NC)\n"
	cargo clippy -p weather-station-py --all-targets -- -D warnings
	@printf "$(GREEN)✓ Clippy clean!$(NC)\n"

## Cross-compile the no_std crates for the embedded target
#
# An MCU station is a stated target for both published crates, and the default
# feature set of a published crate cannot change without a major — so the
# no_std build is verified on every push rather than before a tag.
test-embedded:
	@printf "$(BLUE)Checking no_std cross-compilation for $(EMBEDDED_TARGET)...$(NC)\n"
	@printf "$(YELLOW)  → weather-contracts (schemas only)$(NC)\n"
	cargo check -p weather-contracts --target $(EMBEDDED_TARGET) --target-dir $(EMBEDDED_CHECK_TARGET_DIR) --no-default-features
	@printf "$(YELLOW)  → weather-contracts (linkable + migratable — what a station puts on the wire)$(NC)\n"
	cargo check -p weather-contracts --target $(EMBEDDED_TARGET) --target-dir $(EMBEDDED_CHECK_TARGET_DIR) --no-default-features --features "linkable,migratable"
	@printf "$(YELLOW)  → weather-station (mesh contract without a runtime)$(NC)\n"
	cargo check -p weather-station --target $(EMBEDDED_TARGET) --target-dir $(EMBEDDED_CHECK_TARGET_DIR) --no-default-features
	@printf "$(YELLOW)  → weather-station (no_std + tracing)$(NC)\n"
	cargo check -p weather-station --target $(EMBEDDED_TARGET) --target-dir $(EMBEDDED_CHECK_TARGET_DIR) --no-default-features --features tracing
	@printf "$(GREEN)✓ no_std cross-compilation clean!$(NC)\n"

## Fail if Cargo.lock is stale for the current sibling checkout
#
# The committed lock is a path-dependency lock, so cargo rewrites it whenever
# the sibling aimdb checkout moves. Without this check a fresh clone starts
# dirty and nobody notices which commit the lock actually described.
lockfile:
	@printf "$(GREEN)Checking Cargo.lock is current...$(NC)\n"
	@if ! cargo metadata --locked --format-version 1 >/dev/null; then \
		printf "$(RED)✗ Cargo.lock is stale for the current ../aimdb checkout.$(NC)\n"; \
		printf "$(YELLOW)  Run 'cargo check --workspace' and commit the updated Cargo.lock.$(NC)\n"; \
		exit 1; \
	fi
	@printf "$(GREEN)✓ Cargo.lock is current!$(NC)\n"

## Compile the browser client for the wasm target
#
# The half of `weather-mesh-client` that cannot be built on the host: the
# `createWeatherDb` fusion and everything it drags in from the wasm adapter.
# A separate target dir, for the same reason the embedded check has one.
wasm-check:
	@printf "$(BLUE)Checking $(WASM_TARGET) build of the browser client...$(NC)\n"
	cargo check -p weather-mesh-client --target $(WASM_TARGET) --target-dir $(WASM_CHECK_TARGET_DIR)
	@printf "$(GREEN)✓ Browser client compiles for $(WASM_TARGET)!$(NC)\n"

## Regenerate the TypeScript contract types from the Rust definitions
#
# ts-rs emits at test time. The types are committed so a JS-only contributor can
# typecheck without a Rust toolchain; `ts-bindings-check` is what stops them
# drifting from the contracts they were generated out of.
ts-bindings:
	@printf "$(GREEN)Generating TypeScript contract types...$(NC)\n"
	@mkdir -p $(TS_BINDINGS_DIR)
	TS_RS_EXPORT_DIR=$(CURDIR)/$(TS_BINDINGS_DIR) \
		cargo test -p weather-contracts --features "$(TS_BINDINGS_FEATURES)" export_bindings
	@printf "$(GREEN)✓ Wrote $(TS_BINDINGS_DIR)$(NC)\n"

## Fail if the committed TypeScript types no longer match the Rust contracts
#
# The npm package's whole claim is that its types cannot drift from the wire,
# and that claim is only true if something checks. This is the mesh's version of
# aimdb's `codegen-drift`.
ts-bindings-check:
	@printf "$(GREEN)Checking TypeScript contract types are current...$(NC)\n"
	@rm -rf target/ts-bindings-check && mkdir -p target/ts-bindings-check
	@TS_RS_EXPORT_DIR=$(CURDIR)/target/ts-bindings-check \
		cargo test -q -p weather-contracts --features "$(TS_BINDINGS_FEATURES)" export_bindings >/dev/null
	@if ! diff -ru $(TS_BINDINGS_DIR) target/ts-bindings-check; then \
		printf "$(RED)✗ Committed TypeScript types are stale.$(NC)\n"; \
		printf "$(YELLOW)  Run 'make ts-bindings' and commit the result.$(NC)\n"; \
		exit 1; \
	fi
	@printf "$(GREEN)✓ TypeScript contract types are current!$(NC)\n"

## Typecheck and test the browser client facade
#
# The facade is TypeScript because that is where the adapter's `unknown`
# payloads become contract types — one cast, one place. Its tests run without a
# browser or a wasm build: they drive the facade against a fake module.
js:
	@printf "$(GREEN)Checking the browser client facade...$(NC)\n"
	cd $(JS_DIR) && npm ci
	cd $(JS_DIR) && npm run typecheck
	cd $(JS_DIR) && npm test
	@printf "$(GREEN)✓ Browser client facade clean!$(NC)\n"

## Build the npm package with wasm-pack
#
# Not part of `check`: it needs a wasm-pack install, and nothing downstream of
# it is verified here anyway. `--scope aimdb` sets the package name at build
# time rather than rewriting package.json afterwards; the wasm-pack output is an
# internal artifact that the TypeScript facade wraps, so the published
# package.json is $(JS_DIR)/package.json, not the generated one.
wasm:
	@command -v wasm-pack >/dev/null || { \
		printf "$(RED)✗ wasm-pack not found.$(NC)\n"; \
		printf "$(YELLOW)  Install it: cargo install wasm-pack$(NC)\n"; \
		exit 1; \
	}
	@printf "$(GREEN)Building the browser client...$(NC)\n"
	wasm-pack build weather-mesh-client --target web --scope aimdb --out-dir js/pkg
	cd $(JS_DIR) && npm ci && npm run build
	@printf "$(GREEN)✓ npm package built in $(JS_DIR)$(NC)\n"

## Everything — run before pushing
check: fmt-check clippy test test-embedded wasm-check ts-bindings-check js lockfile
	@printf "$(GREEN)✓ All checks passed!$(NC)\n"

## Remove build artifacts
clean:
	@printf "$(GREEN)Cleaning build artifacts...$(NC)\n"
	cargo clean

## Remove only the browser client build artifacts
clean-wasm:
	@printf "$(GREEN)Cleaning browser client artifacts...$(NC)\n"
	rm -rf $(WASM_CHECK_TARGET_DIR) $(JS_DIR)/pkg $(JS_DIR)/dist $(JS_DIR)/node_modules

## Remove only the embedded check artifacts
clean-embedded:
	@printf "$(GREEN)Cleaning embedded check artifacts...$(NC)\n"
	rm -rf $(EMBEDDED_CHECK_TARGET_DIR)
