# herdr-kharka

Writes what a machine's [herdr](https://herdr.dev) runs into [kharka](https://github.com/jakobbeyer/kharka), on every
change. With it running on every machine, any tool reads the agents of all machines from its local kharka replica
(`kharka watch herdr/`), instead of asking every machine over ssh.

herdr here is **herdr-core** (`hc`), the core of the Herdr app that runs every terminal and agent, on the machine with
the app and on every machine the app reaches over ssh. The herdr terminal multiplexer (herdr's TUI) is no longer read
since 0.3.0.

- **Event-driven:** it follows herdr-core's event stream and looks again on any event about one of this machine's
  terminals, at the latest every 3 s. A subscription that ends (herdr-core restarted) is opened again, a refused one
  after a pause that grows up to a minute.
- **Writes only changes, in a shape of its own:** only what readers need, nothing that changes on every frame; an
  unchanged state is written again every 10 s, so readers can tell it is current.
- **Agents are terminals whose program reports a status** (Claude Code and other agents do); shells are left out. Their
  spaces come from the Herdr app's layout, the agent the app shows from the app's socket where the app runs.
- **Context of Claude sessions:** every Claude agent gets `context_tokens`, the input plus cache tokens of its last
  answer, from its transcript (`~/.claude*/projects/*/<session>.jsonl`; the session from Claude Code's
  `~/.claude*/sessions/<pid>.json`), and every agent the git branch of its folder.
- **Commands from other machines:** with `--commands` it runs the allowed `agent.*` methods other machines write for
  this one into `herdr.commands/<node>/` against herdr-core (or the Herdr app) and answers them there; off by default
  (see [Commands](#commands)).
- **Only this machine's agents:** herdr-core also lists the terminals of machines it reaches (`omarchy/t3`); they are
  left out, each machine writes its own.
- **Steady:** a single failed look (a timeout) keeps the last state; herdr is written as `offline` after three failed
  looks in a row or 10 s, at once only when herdr-core's socket is gone. A write the kharka daemon refuses keeps the
  connection, is logged once and tried again after the heartbeat; a daemon that hangs is given up on after 5 s.

It talks to herdr-core's socket, the Herdr app's socket and the local kharka daemon's socket, nothing else: commands
reach other machines through kharka, without ssh.

## Install

A prebuilt binary (macOS and Linux, arm64 and x86_64; the Linux builds are static):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/jakobbeyer/herdr-kharka/releases/latest/download/herdr-kharka-installer.sh | sh
```

With Homebrew, or from source (Rust 1.88 or newer):

```sh
brew install jakobbeyer/tap/herdr-kharka
cargo install herdr-kharka
```

**Compatibility:** herdr-kharka needs kharka 0.1.0 or newer (the socket protocol) with a running `kharka daemon` on
the machine, and herdr-core (tested with hc 0.1.18 and 0.1.20, Herdr app 0.1.15).

## Run

```sh
herdr-kharka                    # writes herdr/<node>
herdr-kharka --commands         # also runs the commands other machines send to this node
herdr-kharka --commands --allow-method agent.prompt   # and allows one more agent.* method
herdr-kharka --once             # look once, write, end (takes no commands, refuses --commands)
```

As a service, with the arguments given after `service` (options before `service` are refused, and so are arguments
that would not start). macOS (launchd, log in
`~/Library/Logs/herdr-kharka.log`):

```sh
herdr-kharka service > ~/Library/LaunchAgents/dev.kharka.herdr.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.kharka.herdr.plist
```

Linux (systemd user unit, log in `journalctl --user -u herdr-kharka`):

```sh
herdr-kharka service > ~/.config/systemd/user/herdr-kharka.service
systemctl --user daemon-reload && systemctl --user enable --now herdr-kharka
```

A service that should also take commands from other machines needs `service --commands` (and any `--allow-method`);
read [Commands](#commands) and [SECURITY.md](SECURITY.md) first.

Options: `--hc-socket` (default `$HC_SOCKET`, else `$XDG_RUNTIME_DIR/herdr-core/default.sock`, else
`~/.local/state/herdr-core/default/default.sock`), `--app-socket` (default `$HERDR_APP_HOME/herdr-app.sock`, else
`~/.config/herdr-app/herdr-app.sock`), `--kharka-socket` (`$KHARKA_SOCKET`, else kharka's default), `--key` (default
`herdr/{node}`), `--label` (default the node name), `--interval`, `--heartbeat`, `--commands` (run the commands other
machines send, off by default; not with `--once`), `--allow-method` (repeatable, only with `--commands`: allow
`agent.prompt`). `--no-commands` is still taken and does nothing, so that older service files
keep working; it cannot be given together with `--commands`.

## What it writes

`herdr/<node>`, `<node>` being the kharka daemon's node name, about 2 KB for a handful of agents. Its own shape, not
herdr-core's API objects, so that readers do not break when herdr changes:

```json
{
  "host": "server",
  "label": "server",
  "herdr": "online",
  "origin_machine_id": null,
  "focused": "t5",
  "focus_changed": 1791556101230,
  "commands": [],
  "workspaces": [ { "id": "sp3815bbc8a5fb8f2b", "number": 2, "label": "api", "tokens": {} } ],
  "agents": [
    {
      "pane": "t5", "terminal_id": "t5",
      "workspace": "sp3815bbc8a5fb8f2b", "tab": "tb483b9f1a4a8ebe",
      "name": null, "agent": "claude-code",
      "status": "idle", "since": 1791556115452,
      "title": "Merge develop into the branch",
      "session": "0b7e2c4a-5d1f-4e8b-9a36-2f4c8d1e7a90", "context_tokens": 184213,
      "cwd": "/home/me/src/api", "branch": "develop",
      "tokens": {}
    }
  ]
}
```

| field | |
|---|---|
| `label` | the machine's name for people and readers such as herdr-deck (`--label`, else the node name) |
| `origin_machine_id` | always null since 0.3.0 (herdr's TUI reported one) |
| `focused` | the agent the Herdr app shows, if the app runs on this machine and shows one of its agents |
| `focus_changed` | when herdr-kharka last saw `focused` change (ms), else null: which machine is in use right now |
| `commands` | the methods this machine takes from other machines, see [Commands](#commands); else empty |
| `workspaces` | the Herdr app's spaces on this machine, in its order (`number`): the name given, else where its terminals are (as the app calls it), else its folder; `tokens` stays empty |
| `pane`, `terminal_id` | the agent's terminal address in herdr-core (`t5`); other machines reach it as `<machine>/t5` |
| `workspace`, `tab` | the space and tab the app shows the terminal in |
| `name`, `agent` | the terminal's name in herdr-core, if it has one, and the program that reports the status (`claude-code`) |
| `status`, `since` | `working`, `blocked`, `done`, `idle` or `unknown` (any other status), and since when (ms) |
| `title` | the terminal title |
| `session`, `context_tokens` | the Claude session and the tokens its context holds after the last answer |
| `cwd`, `branch` | the agent's folder and the git branch checked out there (`@<sha>` when detached) |
| `tokens` | always empty since 0.3.0 (herdr's TUI had pane tokens) |

While herdr-core cannot be reached: `"herdr": "offline"` with empty lists. Readers should ignore fields they do not
know: new ones are added, none are taken away.

## Commands

Commands are off by default; `herdr-kharka --commands` turns them on for a machine. Then anyone whose machine syncs
with the kharka hub can drive the agents on this machine, and an agent can run shell commands: turn them on only where
you trust every machine on the hub.

A program on any machine (e.g. herdr-deck) has a machine's herdr do something by writing a record for that node,
`herdr.commands/<node>/<id>`:

```json
{ "from": "mac", "at": 1791556120001, "expires": 1791556135001, "method": "agent.send_keys", "params": { "target": "t5", "keys": ["esc"] } }
```

herdr-kharka on that node runs the method against its herdr-core (`agent.focus` against its Herdr app) and writes the
answer into the same record, next to the command:

```json
{ …, "done": { "at": 1791556120240, "ok": true, "result": { … } } }
{ …, "done": { "at": 1791556120240, "ok": false, "error": "agent.focus: no running terminal \"t9\"" } }
```

The sender reads the answer from its watch and deletes the record. herdr-kharka writes the answer only if the record is
unchanged since it read it (a sender that gave up and deleted it gets nothing back), and deletes answers nobody took after
about a minute. A command runs at most once, also when it comes again before its answer shows. An answer the daemon
refuses, e.g. a result larger than a kharka value may be, is replaced by a short error; a command whose short error is
refused too is deleted unanswered after about a minute. A machine without `--commands` does not read or answer its
commands; a sender that gets no answer should delete its record.

- **The methods** keep the names herdr's TUI had; `target` is one of this machine's terminals (`t5`) or its name:

  | method | params | runs |
  |---|---|---|
  | `agent.list` | | a look at herdr-core: `{ agents }` as written above |
  | `agent.get` | `target` | the same, `{ agent }` for one |
  | `agent.read` | `target`, `lines` | herdr-core's `terminal.read`: the screen plus `lines` of history |
  | `agent.send_keys` | `target`, `keys` | herdr-core's `terminal.keys`, e.g. `["esc"]` |
  | `agent.prompt` | `target`, `text` | herdr-core's `terminal.send`: pastes the text and presses Enter |
  | `agent.focus` | `target` | the Herdr app's `agent.focus`: only where the app runs |

- **Only allowed methods run.** By default all of them but `agent.prompt`, which runs only with
  `--allow-method agent.prompt`. Any other method is answered `<method>: not allowed`.
- **The machine says what it takes:** `commands` in `herdr/<node>` lists the allowed methods while the machine takes
  commands (its watch on `herdr.commands/<node>/` is up). It is empty when commands are off or not taken right now, and
  missing before 0.2.0 and from `--once`. A sender can skip machines that do not take the method it wants to send.
- **Limits:** within the last minute, at most 60 commands, at most 30 from one sender (`from`), and at most 20 s that
  herdr spent on them. A command beyond a limit is answered `too many commands, try again later` without running; sent
  again later (a new record, or the same one written again), it is looked at again. Only commands that reached herdr
  count: not those answered without running, nor those that found no herdr-core (it restarts). `from` is not checked: the
  limit per sender holds back a sender that runs wild (e.g. a retry loop), not one that names another sender. The
  limits for all senders together hold either way.
- **Who can send:** every machine whose kharka daemon syncs with the hub, i.e. whoever may reach the hub over ssh;
  kharka has no permissions per node for records. Only machines started with `--commands` take commands.
- **`expires`** (ms) is needed: a command without it is answered `no expires`, one where it is not a number of ms (text,
  below zero) `expires is not a time in ms`, one that comes later, e.g. to a machine that was offline, `expired`; none
  of them runs. A fraction of a ms is cut off. The clocks of the machines should agree to within a few seconds.
- **One after another:** commands run in the order they come, each herdr call taking at most 3 s; a sender should wait
  longer than that for its answer.
- **The id** is one key segment (`A-Z a-z 0-9 . _ -`) that no other command for that node uses; with the sender's node
  in it, unique on the sending machine is enough, e.g. `<node>-<ms>-<counter>`.

By hand, with a watch open in one terminal and the command written in another:

```sh
kharka watch herdr.commands/server/
kharka put herdr.commands/server/me-1 "{\"from\": \"me\", \"at\": $(date +%s)000, \"expires\": $(( $(date +%s) + 30 ))000, \"method\": \"agent.list\"}"
kharka del herdr.commands/server/me-1
```

## The hub's collections

[contrib/](contrib/) has the collections for the hub's `kharka.toml` and their schemas: for `herdr`, each machine
writes only its own key, an entry not refreshed within the TTL reads as stale, and every write must fit
`herdr.schema.json`; any machine writes `herdr.commands`, fitting `herdr.commands.schema.json`. A hub with
`strict = true` needs them, any other one checks the writes against the schemas with them. Copy the three files into
the hub's `~/.config/kharka/` (or add the sections to its `kharka.toml`); the hub reloads them while it runs.

```toml
[collections.herdr]
kind = "state"
ttl = "30s"
schema = "herdr.schema.json"

[collections."herdr.commands"]
kind = "record"
schema = "herdr.commands.schema.json"
```

## Reading it

```sh
kharka ls herdr/                                         # one key per machine
kharka get herdr/server | jq '.agents[] | {name, status, context_tokens}'
kharka watch herdr/ --initial                            # every change, from every machine
```

## Contributing

Issues and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). Report security problems privately, as
[SECURITY.md](SECURITY.md) describes.

## License

MIT, see [LICENSE](LICENSE).
