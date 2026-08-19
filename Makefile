# AimDB Weather Mesh Makefile
#
# Mirrors the aimdb repository's convention: every feature combination CI
# verifies is spelled out here, so a local `make check` and a CI run execute
# the same commands. Adding a feature to a crate means adding its combination
# here — that is the only place the matrix lives.

.PHONY: help build test fmt fmt-check clippy test-embedded lockfile check clean clean-embedded
.DEFAULT_GOAL := help

# Separate target dir for the cross-compile checks, so an interrupted embedded
# build cannot leave .rmeta files that break the next host `cargo check`.
EMBEDDED_CHECK_TARGET_DIR := target/embedded-check
EMBEDDED_TARGET := thumbv7em-none-eabihf

# Every workspace member. `cargo fmt --all` is deliberately not used: it walks
# into path dependencies outside this repository (the sibling aimdb checkout
# and its embassy submodule), which both formats code we do not own and makes
# the result depend on whether that submodule is present.
PACKAGES := weather-contracts weather-station weather-station-openmeteo weather-station-knx weather-hub

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
	@printf "    lockfile       Fail if Cargo.lock is stale for the current sibling checkout\n"
	@printf "    check          Everything above — run before pushing\n"
	@printf "\n"
	@printf "  $(YELLOW)Housekeeping:$(NC)\n"
	@printf "    clean          Remove build artifacts\n"
	@printf "\n"
	@printf "  $(BLUE)Note:$(NC) this workspace reaches the aimdb crates by path, so a sibling\n"
	@printf "        aimdb checkout must exist at ../aimdb.\n"

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
	@printf "$(YELLOW)  → Testing weather-station-openmeteo$(NC)\n"
	cargo test -p weather-station-openmeteo
	@printf "$(YELLOW)  → Testing weather-station-knx$(NC)\n"
	cargo test -p weather-station-knx
	@printf "$(YELLOW)  → Testing weather-hub$(NC)\n"
	cargo test -p weather-hub
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
	@printf "$(YELLOW)  → Clippy on weather-station (no_std, MCU feature set)$(NC)\n"
	cargo clippy -p weather-station --no-default-features -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-station-openmeteo$(NC)\n"
	cargo clippy -p weather-station-openmeteo --all-targets -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-station-knx$(NC)\n"
	cargo clippy -p weather-station-knx --all-targets -- -D warnings
	@printf "$(YELLOW)  → Clippy on weather-hub$(NC)\n"
	cargo clippy -p weather-hub --all-targets -- -D warnings
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

## Everything — run before pushing
check: fmt-check clippy test test-embedded lockfile
	@printf "$(GREEN)✓ All checks passed!$(NC)\n"

## Remove build artifacts
clean:
	@printf "$(GREEN)Cleaning build artifacts...$(NC)\n"
	cargo clean

## Remove only the embedded check artifacts
clean-embedded:
	@printf "$(GREEN)Cleaning embedded check artifacts...$(NC)\n"
	rm -rf $(EMBEDDED_CHECK_TARGET_DIR)
