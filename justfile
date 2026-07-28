set dotenv-load := false

default:
    @just --list

bootstrap-tools:
    cargo install --locked cargo-deny --version 0.20.2
    cargo install --locked cargo-dist --version 0.32.0

fmt:
    cargo fmt --all --check

clippy:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all-targets --all-features

docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

deny:
    cargo deny check

dist-plan:
    dist plan

yaml:
    @find .github -type f \( -name '*.yml' -o -name '*.yaml' \) -print0 | xargs -0 -r -n1 yq eval '.' >/dev/null

ci: fmt clippy test docs deny dist-plan yaml

ci-diff: ci
