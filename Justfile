set shell := ["bash", ".just-shell"]
set positional-arguments

# Show available recipes.
default:
    @just --list

# Refresh the local Tabula table and open it in a browser.
tabula-view:
    @nix run github:timfewi/tabula-repo/a939a9236bc9de28c3ebd30f216f37fbe70f5c7d -- view . --open

# Install Git hooks after reviewing existing configuration.
hooks:
    @python3 .husky-setup.py

# Run declared fast checks; accepts project-check options such as --json.
lint *args:
    @project-check fast "$@"

# Run the declared full gate; accepts project-check options such as --json.
verify *args:
    @project-check full "$@"

# Format supported repository files using pinned tools.
fmt:
    @python3 .project-format.py write rust

# Check formatting without changing files.
fmt-check:
    @python3 .project-format.py check rust

# Alias for the declared fast gate.
check *args:
    @project-check fast "$@"

# Run the application; forwards arguments to the binary.
start *args:
    @cargo run --locked -- "$@"

# Run project tests; accepts cargo test arguments.
test *args:
    @cargo test --locked "$@"

# Build the application.
build *args:
    @cargo build --locked "$@"
