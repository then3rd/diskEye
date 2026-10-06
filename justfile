# Use cargo from PATH, falling back to the rustup install location.
cargo := `command -v cargo || echo ~/.cargo/bin/cargo`

# List recipes
default:
    @just --list

# Build (pass --release for an optimized build)
build *ARGS:
    {{cargo}} build {{ARGS}}

# Run diskeye with the given arguments, e.g. `just run tui` or `just run serve --port 8080`
run *ARGS:
    {{cargo}} run --release -- {{ARGS}}

# One-time setup: pre-commit hook, Playwright + Chromium, and VHS for TUI screenshots
setup:
    command -v pre-commit >/dev/null || uv tool install pre-commit
    pre-commit install
    cd scripts/screenshots && npm install && npx playwright install chromium
    command -v vhs >/dev/null || sudo pacman -S --needed vhs

# Regenerate the README screenshots in docs/screenshots from made-up demo data
screenshots:
    #!/usr/bin/env bash
    set -euo pipefail
    {{cargo}} build
    rm -rf target/demo-state
    # A fixed clock and time zone keep the screenshots reproducible.
    export DISKEYE_NOW=1790865000 TZ=UTC XDG_STATE_HOME=target/demo-state
    export DEMO_SNAPSHOT=$(target/debug/diskeye demo target/demo-state/diskeye/snapshots)
    mkdir -p docs/screenshots
    vhs scripts/screenshots/tui.tape
    node scripts/screenshots/web.mjs target/debug/diskeye "$DEMO_SNAPSHOT" docs/screenshots
