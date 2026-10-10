# Contributing

For anything larger than a small fix, please open an issue first, so we agree on the direction before you spend time
on it.

## Build and check

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the same three on Linux and macOS. The tests use a fake herdr and a fake kharka daemon on Unix sockets, so they
need neither installed. The minimum supported Rust version is the `rust-version` in `Cargo.toml`; a job in CI tests
with it.

## Output and commands

What herdr-kharka writes (`herdr/<node>`) and the commands records are read and written by other programs. Adding a
field is fine; removing or renaming one is not. A change to either must update the README and the schemas in
[contrib/](contrib/). Say in the pull request how existing readers and older versions are affected.

## Docs

Write short, plain sentences. No marketing, no filler sections. Keep examples free of real machine names, hosts and
paths: use names like `server` and `laptop`.

## License

herdr-kharka is MIT licensed. By contributing, you agree that your contribution is under the same license.
