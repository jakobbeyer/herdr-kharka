# herdr-kharka

Writes what a machine's [herdr](https://herdr.dev) runs into [kharka](https://github.com/jakobbeyer/kharka), on every
change. With it running on every machine, any tool reads the agents of all machines from its local kharka replica
(`kharka watch herdr/`), instead of asking every machine over ssh.

- **Event-driven:** it follows herdr's event stream and looks again on any event (an agent changes status, a pane
  opens, closes or moves, a workspace is renamed, moved or gets new tokens), at the latest every 3 s. A subscription
  that ends (herdr restarted) is opened again, a refused one after a pause that grows up to a minute; an older herdr
  that does not know every event gets the ones it knows.
- **Writes only changes, in a shape of its own:** only what readers need, nothing that changes on every frame (the
  animated terminal title, the pane token `spin`); an unchanged state is written again every 10 s, so readers can tell
  it is current.
- **Context of Claude sessions:** every Claude agent gets `context_tokens`, the input plus cache tokens of its last
  answer, from its transcript (`~/.claude*/projects/*/<session>.jsonl`), and every agent the git branch of its folder.
- **Commands from other machines:** it runs the `agent.*` methods other machines write for this one into
  `herdr.commands/<node>/` against the local herdr and answers them there (see [Commands](#commands)).
- **Only this machine's agents:** a herdr that lists agents and workspaces of other machines too (e.g. a fork with
  federation) has them left out; each machine writes its own.
- **Steady:** a single failed look at herdr (a timeout) keeps the last state; herdr is written as `offline` after three
  failed looks in a row or 10 s, at once only when its socket is gone. A write the kharka daemon refuses keeps the
  connection, is logged once and tried again after the heartbeat; a daemon that hangs is given up on after 5 s.

It talks to herdr's socket and to the local kharka daemon's socket, nothing else: commands reach other machines
through kharka, without ssh.

## Install and run

Needs Rust 1.85 or newer to build, a running `kharka daemon` on the machine and herdr (tested with 0.9.3).

```sh
cargo install --git https://github.com/jakobbeyer/herdr-kharka
herdr-kharka                    # writes herdr/<node>, runs commands for this node
herdr-kharka --once             # look once, write, end
```

As a service, with the arguments given after `service`. macOS (launchd, log in `~/Library/Logs/herdr-kharka.log`):

```sh
herdr-kharka service > ~/Library/LaunchAgents/dev.kharka.herdr.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.kharka.herdr.plist
```

Linux (systemd user unit, log in `journalctl --user -u herdr-kharka`):

```sh
herdr-kharka service > ~/.config/systemd/user/herdr-kharka.service
systemctl --user daemon-reload && systemctl --user enable --now herdr-kharka
```

Options: `--herdr-socket` (default `$HERDR_SOCKET_PATH`, else `~/.config/herdr/herdr.sock`), `--kharka-socket`
(`$KHARKA_SOCKET`, else kharka's default), `--key` (default `herdr/{node}`), `--label` (default the node name),
`--interval`, `--heartbeat`, `--ignore-token` (repeatable, default `spin`), `--no-commands` (run no commands).

## What it writes

`herdr/<node>`, `<node>` being the kharka daemon's node name, about 2 KB for a handful of agents. Its own shape, not
herdr's API objects, so that readers do not break when herdr changes:

```json
{
  "host": "server",
  "label": "server",
  "herdr": "online",
  "origin_machine_id": "machine_5d1f4e8b",
  "focused": "w0:p1",
  "focus_changed": 1791556101230,
  "workspaces": [ { "id": "w0", "number": 2, "label": "api", "tokens": { "tests": "◌ 3/40" } } ],
  "agents": [
    {
      "pane": "w0:p1", "terminal_id": "term_65d12f.2", "display_agent": "C2", "origin_machine_id": "machine_5d1f4e8b",
      "workspace": "w0", "tab": "w0:t1",
      "name": "api-1", "agent": "claude",
      "status": "idle", "since": 1791556115452,
      "title": "Merge develop into the branch",
      "session": "0b7e2c4a-5d1f-4e8b-9a36-2f4c8d1e7a90", "context_tokens": 184213,
      "cwd": "/home/me/src/api", "branch": "develop",
      "tokens": { "repo": "github.com/example/api" }
    }
  ]
}
```

| field | |
|---|---|
| `label` | the machine's name for people and readers such as bhote (`--label`, else the node name) |
| `origin_machine_id` | herdr's id of this installation, if herdr reports one |
| `focused` | the pane the herdr TUI shows, if it is an agent |
| `focus_changed` | when herdr-kharka last saw `focused` change (ms), else null: which machine is in use right now |
| `workspaces` | in the TUI's order (`number`), with the label herdr shows and the workspace's tokens |
| `terminal_id` | herdr's id of the agent's terminal, which stays when its pane moves |
| `display_agent` | the short name herdr shows for the agent, e.g. `C2` |
| `status`, `since` | `working`, `blocked`, `done`, `idle` or `unknown` (any other herdr status), and since when (ms) |
| `title` | the terminal title without its animated prefix |
| `session`, `context_tokens` | the Claude session and the tokens its context holds after the last answer |
| `cwd`, `branch` | the agent's folder and the git branch checked out there (`@<sha>` when detached) |
| `tokens` | the pane's tokens (on a workspace: the workspace's), text only, without the ones given to `--ignore-token` |

While herdr cannot be reached: `"herdr": "offline"` with empty lists. Readers should ignore fields they do not know:
new ones are added, none are taken away.

## Commands

A program on any machine (e.g. herdr-deck) has a machine's herdr do something by writing a record for that node,
`herdr.commands/<node>/<id>`:

```json
{ "from": "mac", "at": 1791556120001, "expires": 1791556135001, "method": "agent.focus", "params": { "target": "w0:p1" } }
```

herdr-kharka on that node runs the method against its herdr and writes the answer into the same record, next to the
command:

```json
{ …, "done": { "at": 1791556120240, "ok": true, "result": { … } } }
{ …, "done": { "at": 1791556120240, "ok": false, "error": "agent.focus: pane not found" } }
```

The sender reads the answer from its watch and deletes the record. herdr-kharka writes the answer only if the record is
unchanged since it read it (a sender that gave up and deleted it gets nothing back), and deletes answers nobody took after
about a minute.

- **Every `agent.*` method** runs (focus, rename, send keys, prompt …), any other one is answered `not allowed`.
- **Who can send:** every machine whose kharka daemon syncs with the hub, i.e. whoever may reach the hub over ssh;
  kharka has no permissions per node for records. `--no-commands` turns commands off on a machine.
- **`expires`** (ms): a command that comes later, e.g. to a machine that was offline, is answered `expired` and does
  not run. The clocks of the machines should agree to within a few seconds.
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

## License

MIT, see [LICENSE](LICENSE).
