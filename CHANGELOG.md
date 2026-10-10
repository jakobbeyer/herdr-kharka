# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project follows [Semantic Versioning](https://semver.org/). Before 1.0, a minor release may still change what is
written or how commands work; the notes say so when it does.

## [0.1.0] - 2026-10-10

The first release.

### Added

- Writes what a machine's herdr runs (agents, workspaces, focus) into kharka as `herdr/<node>`, in a shape of its
  own that does not change when herdr's API does. It needs kharka 0.1.0 or newer (the socket protocol) and herdr
  (tested with 0.9.3).
- Event-driven: it follows herdr's event stream and looks again on any event, at the latest every 3 s. A subscription
  that ends is opened again; an older herdr that does not know every event gets the ones it knows.
- Writes only changes, without what changes on every frame (the animated terminal title, the pane token `spin`). An
  unchanged state is written again every 10 s, so readers can tell it is current.
- `context_tokens` for every Claude agent, from its transcript under `~/.claude*/projects`, and the git branch of
  every agent's folder. Only token counts are written, never content.
- Only this machine's agents: agents and workspaces of other machines that a herdr lists are left out.
- A steady connection: a single failed look at herdr keeps the last state, herdr is written as `offline` after three
  failed looks in a row or 10 s, and a write the kharka daemon refuses is logged once and tried again.
- Commands from other machines: the `agent.*` methods written to `herdr.commands/<node>/` run against the local herdr
  and are answered in the same record. They are off by default and turned on with `--commands` (see
  [SECURITY.md](SECURITY.md)).
- `herdr-kharka service` prints a launchd or systemd unit.
- Schemas and collection settings for the hub in `contrib/`.
- Prebuilt binaries for macOS and Linux (arm64, x86_64), a shell installer and a Homebrew formula
  (`jakobbeyer/tap`). The minimum supported Rust version is 1.88.

### Changed

For those who ran a build from before this release:

- Commands are now off by default. A service that should keep running them needs `--commands`: make it again with
  `herdr-kharka service --commands …`, or add the flag to the `ProgramArguments` (launchd) or `ExecStart` (systemd) of
  the existing one.
- `--no-commands` is still taken and does nothing, so that older service files keep starting. It cannot be given
  together with `--commands`.

[0.1.0]: https://github.com/jakobbeyer/herdr-kharka/releases/tag/v0.1.0
