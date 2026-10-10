# Security

## Supported versions

Only the latest 0.x release gets security fixes.

## Reporting a vulnerability

Please report privately, not in a public issue. Use GitHub's private vulnerability reporting: open the repository's
Security tab and press "Report a vulnerability".

Please include:

- the herdr-kharka version (`herdr-kharka --version`), the kharka and herdr versions, and the operating system;
- what you did, what you expected and what happened, with the smallest steps or input that show it;
- what an attacker gains from it, if you know.

herdr-kharka is a one-person project. I reply as time allows, and I cannot promise a deadline. I will tell you whether
I consider the report a vulnerability, and I will credit you in the fix if you want that.

## Security model

- **It talks to two local sockets and nothing else:** the local herdr socket and the local kharka daemon's socket. It
  opens no port and makes no network connection of its own. What reaches other machines goes through kharka.
- **It reads Claude transcripts to count tokens.** It reads `~/.claude*/projects/*/<session>.jsonl` to find the context
  size of a session. It writes only token counts, never the content of a transcript.
- **What it writes is readable by every machine on the hub.** `herdr/<node>` holds agent names, terminal titles,
  folders, git branches and pane tokens. Do not sync it to machines that should not see these.
- **Commands are off by default.** With `--commands`, every machine that syncs with the kharka hub can run the allowed
  herdr methods on this machine. That includes sending keys to agents, and an agent can run shell commands. kharka has
  no permissions per node for records, and the sender a command names (`from`) is not checked. Turn commands on only
  where every machine on the hub is trusted. `--no-commands` from older service files is taken and does nothing.
- **Only listed methods run.** The allowlist is fixed by name; the README's Commands section lists it, and
  `herdr/<node>` shows it as `commands`. It includes sending keys and reading a pane's output (`agent.read`). A method a
  later herdr adds is not allowed until it is named with `--allow-method`, which takes only `agent.*` names.
- **Commands are limited.** Within a minute at most 60 commands run, at most 30 per sender, and herdr spends at most
  about 20 s on them; any more are answered without running. The sender (`from`) is not checked, so a hostile machine
  can name other senders and get past the limit per sender, and still holds up all senders until the minute is over.
  The limits for all senders together hold in any case: herdr spends at most about a third of each minute on commands.
- **The log quotes what a sender wrote.** A command's method, sender and error are logged quoted and escaped, so that a
  command cannot add lines of its own to the log.
