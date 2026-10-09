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
- **Only this machine's agents:** agents and workspaces herdr federates from other machines are left to their own
  machine.
- **Steady:** a single failed look at herdr (a timeout) keeps the last state; herdr is written as `offline` after three
  failed looks in a row or 10 s, at once only when its socket is gone. A write the kharka daemon refuses keeps the
  connection, is logged once and tried again after the heartbeat; a daemon that hangs is given up on after 5 s.

It talks to herdr's socket and to the local kharka daemon's socket, nothing else.

## Install and run

```sh
cargo install --git https://github.com/jakobbeyer/herdr-kharka
herdr-kharka                    # needs a running `kharka daemon`; writes herdr/<node>
herdr-kharka --once             # look once, write, end
```

As a service (launchd on macOS, systemd on Linux):

```sh
herdr-kharka service > ~/Library/LaunchAgents/dev.kharka.herdr.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.kharka.herdr.plist
```

Options: `--herdr-socket` (default `$HERDR_SOCKET_PATH`, else `~/.config/herdr/herdr.sock`), `--kharka-socket`
(`$KHARKA_SOCKET`, else kharka's default), `--key` (default `herdr/{node}`), `--label` (default the node name),
`--interval`, `--heartbeat`, `--ignore-token` (repeatable, default `spin`).

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
| `origin_machine_id` | herdr's id of this machine; federated herdrs show it on this machine's agents |
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

[contrib/](contrib/) has the collection for the hub's `kharka.toml` and its schema, `herdr.schema.json`: each machine
writes only its own key, an entry not refreshed within the TTL reads as stale, and every write must fit the schema.

```toml
[collections.herdr]
kind = "state"
ttl = "30s"
schema = "herdr.schema.json"
```

## Reading it

```sh
kharka ls herdr/                                         # one key per machine
kharka get herdr/server | jq '.agents[] | {name, status, context_tokens}'
kharka watch herdr/ --initial                            # every change, from every machine
```

## License

MIT, see [LICENSE](LICENSE).
