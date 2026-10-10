//! herdr's API socket: one request per connection, plus one connection that stays open for events.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};

use crate::context::{Transcripts, session_of};

/// Events that change what herdr-kharka writes; agent status changes are subscribed per pane. `pane.updated` is left
/// out: it comes with every frame of an animated title.
const GLOBAL_EVENTS: [&str; 17] = [
    "pane.created",
    "pane.closed",
    "pane.exited",
    "pane.moved",
    "pane.focused",
    "pane.agent_detected",
    "workspace.created",
    "workspace.renamed",
    "workspace.moved",
    "workspace.reordered",
    "workspace.metadata_updated",
    "workspace.focused",
    "workspace.closed",
    "tab.created",
    "tab.closed",
    "tab.moved",
    "tab.focused",
];
/// The events an older herdr knows: it refuses a whole subscription that names one it does not know.
const LEGACY_EVENTS: [&str; 8] = [
    "pane.created",
    "pane.closed",
    "pane.exited",
    "pane.moved",
    "pane.focused",
    "pane.agent_detected",
    "workspace.renamed",
    "workspace.closed",
];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// The longest wait before a refused subscription is asked for again.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A subscription that lasted this long ended for a reason of its own (herdr restarted), not because herdr refuses it.
const STEADY: Duration = Duration::from_secs(10);

/// herdr's default socket: `$HERDR_SOCKET_PATH`, else `~/.config/herdr/herdr.sock`.
pub fn default_socket() -> PathBuf {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/herdr/herdr.sock")
}

/// What herdr runs on this machine, in the shape herdr-kharka writes (see the README): only what readers need, and
/// nothing that changes on every frame.
pub struct Look {
    /// herdr's id of this machine, which federated readers see as an agent's `origin_machine_id`.
    pub origin_machine_id: Option<String>,
    pub agents: Vec<Value>,
    pub workspaces: Vec<Value>,
    /// The pane the herdr TUI shows, if it shows an agent.
    pub focused: Option<String>,
    /// This machine's agent panes, sorted: their status events need a subscription each.
    pub panes: Vec<String>,
}

/// The statuses readers know; herdr's others become `unknown`.
const STATUSES: [&str; 5] = ["working", "blocked", "done", "idle", "unknown"];

/// The agents and workspaces of this machine's herdr. Federated agents and workspaces (of other machines) are left out,
/// so are the pane tokens in `ignore_tokens`; Claude agents get `context_tokens`, every agent the git branch of its
/// folder.
pub fn look(socket: &Path, ignore_tokens: &[String], transcripts: &mut Transcripts) -> anyhow::Result<Look> {
    let agent_list = request(socket, "agent.list")?;
    let workspace_list = request(socket, "workspace.list")?;
    let mut focused = None;
    let agents: Vec<Value> = agent_list["agents"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|raw| is_local(raw))
        .filter_map(|raw| {
            let agent = agent(raw, ignore_tokens, transcripts)?;
            if raw["focused"] == true {
                focused = Some(agent["pane"].clone());
            }
            Some(agent)
        })
        .collect();
    let mut panes: Vec<String> = agents
        .iter()
        .filter_map(|agent| agent["pane"].as_str().map(str::to_string))
        .collect();
    panes.sort();
    let workspaces = workspace_list["workspaces"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|raw| is_local(raw))
        .map(|raw| {
            json!({
                "id": text(raw, "workspace_id").unwrap_or_default(),
                "number": raw["number"].as_u64().unwrap_or(0),
                "label": text(raw, "label").unwrap_or_default(),
                "tokens": tokens_of(raw, ignore_tokens),
            })
        })
        .collect();
    Ok(Look {
        origin_machine_id: text(&agent_list, "origin_machine_id"),
        agents,
        workspaces,
        focused: focused.and_then(|pane| pane.as_str().map(str::to_string)),
        panes,
    })
}

/// Whether an agent or workspace runs on this machine: herdr marks the federated ones with the machine they come from.
fn is_local(raw: &Value) -> bool {
    raw.get("machine_profile_id")
        .or_else(|| raw.get("machine_id"))
        .is_none_or(Value::is_null)
}

/// One agent as herdr-kharka writes it; none without a pane.
fn agent(raw: &Value, ignore_tokens: &[String], transcripts: &mut Transcripts) -> Option<Value> {
    let pane = text(raw, "pane_id")?;
    let status = text(raw, "agent_status")
        .filter(|status| STATUSES.contains(&status.as_str()))
        .unwrap_or_else(|| "unknown".to_string());
    let cwd = text(raw, "foreground_cwd").or_else(|| text(raw, "cwd"));
    let tokens = tokens_of(raw, ignore_tokens);
    let session = raw.as_object().and_then(session_of);
    let context = session.as_deref().and_then(|session| transcripts.context(session));
    Some(json!({
        "pane": pane,
        "terminal_id": text(raw, "terminal_id"),
        "display_agent": text(raw, "display_agent"),
        "origin_machine_id": text(raw, "origin_machine_id"),
        "workspace": text(raw, "workspace_id").unwrap_or_default(),
        "tab": text(raw, "tab_id").unwrap_or_default(),
        "name": text(raw, "name"),
        "agent": text(raw, "agent").unwrap_or_default(),
        "status": status,
        "since": raw["status_since_unix_ms"].as_u64().unwrap_or(0),
        "title": text(raw, "terminal_title_stripped").filter(|title| !title.is_empty()),
        "session": session,
        "context_tokens": context,
        "cwd": cwd.clone().unwrap_or_default(),
        "branch": cwd.as_deref().and_then(|cwd| branch_of(Path::new(cwd))),
        "tokens": tokens,
    }))
}

/// The tokens of a pane or workspace (set with `herdr … report-metadata`), without the ignored ones and without values
/// that are not text.
fn tokens_of(raw: &Value, ignore_tokens: &[String]) -> Map<String, Value> {
    let mut tokens = raw["tokens"].as_object().cloned().unwrap_or_default();
    tokens.retain(|name, value| value.is_string() && !ignore_tokens.contains(name));
    tokens
}

fn text(raw: &Value, field: &str) -> Option<String> {
    raw[field].as_str().map(str::to_string)
}

/// The branch checked out in the repository around `folder` (worktrees too), `@<short sha>` when detached; read from
/// `.git`, without running git.
pub fn branch_of(folder: &Path) -> Option<String> {
    let head = folder.ancestors().find_map(|dir| {
        let git = dir.join(".git");
        if git.is_dir() {
            return Some(git.join("HEAD"));
        }
        let pointer = std::fs::read_to_string(&git).ok()?;
        let target = Path::new(pointer.trim().strip_prefix("gitdir: ")?);
        Some(dir.join(target).join("HEAD"))
    })?;
    let head = std::fs::read_to_string(head).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        None if head.len() >= 7 && head.bytes().all(|b| b.is_ascii_hexdigit()) => Some(format!("@{}", &head[..7])),
        None => None,
    }
}

/// One request on its own connection, as herdr wants it.
fn request(socket: &Path, method: &str) -> anyhow::Result<Value> {
    call(socket, method, &json!({}))
}

/// Any method with its params, on its own connection: the result, or herdr's error message.
pub fn call(socket: &Path, method: &str, params: &Value) -> anyhow::Result<Value> {
    let mut stream = UnixStream::connect(socket).with_context(|| format!("no herdr at {}", socket.display()))?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    writeln!(
        stream,
        "{}",
        json!({ "id": "herdr-kharka", "method": method, "params": params })
    )?;
    for line in BufReader::new(stream).lines() {
        let message: Value = serde_json::from_str(&line.with_context(|| format!("{method}: no answer"))?)?;
        if message["id"] != "herdr-kharka" {
            continue;
        }
        if let Some(error) = message.get("error") {
            match error["message"].as_str() {
                Some(text) => bail!("{method}: {text}"),
                None => bail!("{method}: {error}"),
            }
        }
        return Ok(message["result"].clone());
    }
    bail!("{method}: herdr closed the connection")
}

/// herdr's events, followed for the agent panes of the last look: subscribed again when the panes change or when the
/// subscription ends (herdr restarted, closed or refused it); after a refusal or an early end with a pause that grows up
/// to a minute.
#[derive(Default)]
pub struct Events {
    current: Option<Subscription>,
    /// Only the events an older herdr knows, after it refused the others.
    legacy: bool,
    refusals: u32,
    retry_at: Option<Instant>,
    problem: Option<String>,
}

impl Events {
    /// Keeps a subscription for `panes` open, as far as herdr lets it.
    pub fn follow(&mut self, socket: &Path, panes: &[String], wake: &mpsc::Sender<()>) {
        if let Some(subscription) = &self.current {
            let health = &subscription.health;
            let steady = health.started.load(Ordering::SeqCst) && subscription.opened.elapsed() >= STEADY;
            if health.alive.load(Ordering::SeqCst) {
                if health.started.load(Ordering::SeqCst) {
                    self.problem = None;
                }
                if steady {
                    self.refusals = 0;
                }
                if subscription.panes == panes {
                    return;
                }
            } else {
                let refusal = health.refusal.lock().expect("not poisoned").take();
                if !self.legacy
                    && refusal
                        .as_deref()
                        .is_some_and(|refusal| refusal.contains("unknown variant"))
                {
                    self.legacy = true;
                } else if refusal.is_none() && steady {
                    // herdr went away after a while (a restart): subscribe again at once
                    self.refusals = 0;
                } else {
                    self.refusals += 1;
                    let pause = Duration::from_secs(1 << (self.refusals - 1).min(6)).min(MAX_BACKOFF);
                    self.retry_at = Some(Instant::now() + pause);
                    self.problem = Some(match refusal {
                        Some(refusal) => format!("herdr refused the event subscription: {refusal}"),
                        None => "herdr closed the event subscription".to_string(),
                    });
                }
            }
        }
        // the old subscription is closed before a new one is opened
        self.current = None;
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        self.retry_at = None;
        let events: &[&str] = if self.legacy { &LEGACY_EVENTS } else { &GLOBAL_EVENTS };
        match Subscription::open(socket, events, panes.to_vec(), wake.clone()) {
            Ok(subscription) => self.current = Some(subscription),
            Err(err) => self.problem = Some(format!("no herdr events: {err:#}")),
        }
    }

    /// When a refused subscription is due again.
    pub fn retry_at(&self) -> Option<Instant> {
        self.retry_at
    }

    /// Why there are no events, while there are none.
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }
}

/// The open `events.subscribe` connection; any event wakes the collector, and so does its end. Dropping it closes the
/// connection, which ends its reading thread.
struct Subscription {
    panes: Vec<String>,
    opened: Instant,
    stream: UnixStream,
    health: Arc<Health>,
}

/// What the reading thread knows about its subscription.
#[derive(Default)]
struct Health {
    alive: AtomicBool,
    /// herdr confirmed the subscription.
    started: AtomicBool,
    /// herdr's answer when it refused the subscription.
    refusal: Mutex<Option<String>>,
}

impl Subscription {
    fn open(
        socket: &Path,
        events: &[&str],
        panes: Vec<String>,
        wake: mpsc::Sender<()>,
    ) -> anyhow::Result<Subscription> {
        let mut stream = UnixStream::connect(socket).with_context(|| format!("no herdr at {}", socket.display()))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let subscriptions: Vec<Value> = events
            .iter()
            .map(|kind| json!({ "type": kind }))
            .chain(
                panes
                    .iter()
                    .map(|pane| json!({ "type": "pane.agent_status_changed", "pane_id": pane })),
            )
            .collect();
        let request = json!({ "id": "herdr-kharka-events", "method": "events.subscribe", "params": { "subscriptions": subscriptions } });
        writeln!(stream, "{request}")?;
        let reader = stream.try_clone()?;
        let health = Arc::new(Health {
            alive: AtomicBool::new(true),
            ..Health::default()
        });
        let seen = health.clone();
        thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(error) = message.get("error") {
                    let error = error["message"]
                        .as_str()
                        .map_or_else(|| error.to_string(), str::to_string);
                    *seen.refusal.lock().expect("not poisoned") = Some(error);
                    break;
                }
                if message.get("result").is_some() {
                    seen.started.store(true, Ordering::SeqCst);
                }
                if message.get("event").is_some() && wake.send(()).is_err() {
                    break;
                }
            }
            // ended by herdr, not dropped: the collector looks at once and subscribes again
            if seen.alive.swap(false, Ordering::SeqCst) {
                let _ = wake.send(());
            }
        });
        Ok(Subscription {
            panes,
            opened: Instant::now(),
            stream,
            health,
        })
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.health.alive.store(false, Ordering::SeqCst);
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches_of_repositories_and_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/login\n").unwrap();
        assert_eq!(branch_of(&repo.join("src/deep")).as_deref(), Some("feature/login"));

        let worktree = dir.path().join("wt");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::write(repo.join(".git/worktrees/wt/HEAD"), "1234567890abcdef\n").unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        assert_eq!(branch_of(&worktree).as_deref(), Some("@1234567"));
        assert_eq!(branch_of(dir.path()), None);
    }
}
