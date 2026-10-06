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
