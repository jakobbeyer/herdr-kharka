//! herdr-core (`hc`), the core that runs every terminal and agent of the Herdr app: one JSON-RPC request per
//! connection, plus one connection that stays open for events. And the Herdr app's own socket, which can show an agent.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::context::{Transcripts, session_of_pid};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// The longest wait before a refused subscription is asked for again.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A subscription that lasted this long ended for a reason of its own (the core restarted), not because it is refused.
const STEADY: Duration = Duration::from_secs(10);
/// The layout document of the Herdr app: its spaces and their tabs.
const LAYOUT_DOC: &str = "herdr-app";

/// hc's socket: `$HC_SOCKET`, else `$XDG_RUNTIME_DIR/herdr-core/default.sock` (Linux), else
/// `~/.local/state/herdr-core/default/default.sock` (macOS).
pub fn default_socket() -> PathBuf {
    if let Some(path) = env_path("HC_SOCKET") {
        return path;
    }
    if let Some(runtime) = env_path("XDG_RUNTIME_DIR") {
        return runtime.join("herdr-core/default.sock");
    }
    home().join(".local/state/herdr-core/default/default.sock")
}

/// The Herdr app's socket: `$HERDR_APP_HOME/herdr-app.sock`, else `~/.config/herdr-app/herdr-app.sock`.
pub fn default_app_socket() -> PathBuf {
    env_path("HERDR_APP_HOME")
        .unwrap_or_else(|| home().join(".config/herdr-app"))
        .join("herdr-app.sock")
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// What the core runs on this machine, in the shape herdr-kharka writes (see the README): only what readers need, and
/// nothing that changes on every frame.
pub struct Look {
    pub agents: Vec<Value>,
    pub workspaces: Vec<Value>,
    /// The agent the Herdr app shows, if it runs here and shows one of this machine's agents.
    pub focused: Option<String>,
}

/// The statuses readers know; others become `unknown`.
const STATUSES: [&str; 5] = ["working", "blocked", "done", "idle", "unknown"];

/// The agents (terminals whose program reports a status) and spaces of this machine's core. Terminals of other
/// machines (`<machine>/t3`) are left out; Claude agents get `context_tokens`, every agent the git branch of its folder.
pub fn look(socket: &Path, app_socket: &Path, transcripts: &mut Transcripts) -> anyhow::Result<Look> {
    let terminals = call(socket, "terminal.list", &json!({}))?;
    let terminals: Vec<&Value> = terminals["terminals"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|raw| raw["address"].as_str().is_some_and(is_local))
        .collect();
    // spaces are the app's; without its layout (no app ever ran here) there are none
    let layout = call(socket, "doc.get", &json!({ "name": LAYOUT_DOC })).unwrap_or(Value::Null);
    let workspaces = workspaces(&layout["value"], &terminals);
    let agents: Vec<Value> = terminals.iter().filter_map(|raw| agent(raw, transcripts)).collect();
    let focused = app_status(app_socket)
        .ok()
        .and_then(|status| status["agent"].as_str().map(str::to_string))
        .filter(|address| agents.iter().any(|agent| agent["pane"] == address.as_str()));
    Ok(Look {
        agents,
        workspaces,
        focused,
    })
}

/// Whether a terminal address is this machine's: other machines' have their name in front (`omarchy/t3`).
pub fn is_local(address: &str) -> bool {
    !address.is_empty() && !address.contains('/')
}

/// The spaces of the app's layout, in its order: a space nobody named is called after where its terminals are (the
/// app's `where` label, `bilendo / 1`), else after its folder.
fn workspaces(layout: &Value, terminals: &[&Value]) -> Vec<Value> {
    let mut whereabouts: HashMap<&str, &str> = HashMap::new();
    for raw in terminals {
        let labels = &raw["labels"];
        if let (Some(space), Some(place)) = (labels["herdr.space"].as_str(), labels["where"].as_str()) {
            whereabouts
                .entry(space)
                .or_insert_with(|| place.split(" / ").next().unwrap_or(place));
        }
    }
    layout["spaces"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|space| {
            let id = space["id"].as_str()?;
            let name = space["name"].as_str().filter(|name| !name.is_empty());
            let folder = space["folder"]
                .as_str()
                .and_then(|folder| Path::new(folder).file_name())
                .map(|name| name.to_string_lossy().into_owned());
            let label = name
                .map(str::to_string)
                .or_else(|| whereabouts.get(id).map(|place| place.to_string()))
                .or(folder)
                .unwrap_or_default();
            Some((id.to_string(), label))
        })
        .enumerate()
        .map(|(n, (id, label))| json!({ "id": id, "number": n + 1, "label": label, "tokens": {} }))
        .collect()
}

/// One agent as herdr-kharka writes it; none for a terminal whose program reports no status (a shell).
fn agent(raw: &Value, transcripts: &mut Transcripts) -> Option<Value> {
    let address = raw["address"].as_str()?;
    let status = &raw["status"];
    let state = status["state"].as_str()?;
    let state = if STATUSES.contains(&state) { state } else { "unknown" };
    let foreground = &raw["foreground"];
    let cwd = text(foreground, "cwd").or_else(|| text(raw, "cwd"));
    let session = foreground["pid"].as_u64().and_then(session_of_pid);
    let context = session.as_deref().and_then(|session| transcripts.context(session));
    let labels = &raw["labels"];
    Some(json!({
        "pane": address,
        "terminal_id": address,
        "workspace": text(labels, "herdr.space").unwrap_or_default(),
        "tab": text(labels, "herdr.tab").unwrap_or_default(),
        "name": text(raw, "name"),
        "agent": text(status, "app").unwrap_or_default(),
        "status": state,
        "since": raw["status_ms"].as_u64().unwrap_or(0),
        "title": text(raw, "title").filter(|title| !title.is_empty()),
        "session": session,
        "context_tokens": context,
        "cwd": cwd.clone().unwrap_or_default(),
        "branch": cwd.as_deref().and_then(|cwd| branch_of(Path::new(cwd))),
        "tokens": {},
    }))
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

/// A call that did not reach the core: its socket is gone or nobody listens there (it restarts).
#[derive(Debug)]
pub struct NoCore(PathBuf);

impl std::fmt::Display for NoCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no herdr-core at {}", self.0.display())
    }
}

/// Any method with its params, on its own connection: the result, or the core's error; [`NoCore`] when it could not
/// connect.
pub fn call(socket: &Path, method: &str, params: &Value) -> anyhow::Result<Value> {
    let mut stream = UnixStream::connect(socket).with_context(|| NoCore(socket.to_path_buf()))?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    writeln!(
        stream,
        "{}",
        json!({ "jsonrpc": "2.0", "id": "herdr-kharka", "method": method, "params": params })
    )?;
    for line in BufReader::new(stream).lines() {
        let message: Value = serde_json::from_str(&line.with_context(|| format!("{method}: no answer"))?)?;
        if message["id"] != "herdr-kharka" {
            continue;
        }
        if let Some(error) = message.get("error") {
            bail!("{method}: {}", error_text(error));
        }
        return Ok(message["result"].clone());
    }
    bail!("{method}: herdr-core closed the connection")
}

/// The core's error: its message, and the kind of error when it gives one.
fn error_text(error: &Value) -> String {
    let message = error["message"]
        .as_str()
        .map_or_else(|| error.to_string(), str::to_string);
    match error["data"]["kind"].as_str() {
        Some(kind) if !message.contains(kind) => format!("{message} ({kind})"),
        _ => message,
    }
}

/// What the Herdr app shows (`{ status }` on its socket): the space, the focused pane, the agent.
fn app_status(app_socket: &Path) -> anyhow::Result<Value> {
    app_request(app_socket, &json!({ "status": {} }))
}

/// Runs one of the Herdr app's commands (`herdr-app commands`), e.g. `agent.focus` with `{ terminal }`.
pub fn app_do(app_socket: &Path, command: &str, args: &Value) -> anyhow::Result<Value> {
    app_request(app_socket, &json!({ "do": command, "args": args })).with_context(|| command.to_string())
}

fn app_request(app_socket: &Path, request: &Value) -> anyhow::Result<Value> {
    let mut stream = UnixStream::connect(app_socket).context("the Herdr app does not run on this machine")?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    writeln!(stream, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("the Herdr app did not answer")?;
    let answer: Value = serde_json::from_str(&line).context("the Herdr app answered something else")?;
    if let Some(error) = answer["error"].as_str() {
        bail!("{error}");
    }
    Ok(answer)
}

/// The core's events, followed on one connection: subscribed again when the subscription ends (the core restarted,
/// closed or refused it); after a refusal or an early end with a pause that grows up to a minute.
#[derive(Default)]
pub struct Events {
    current: Option<Subscription>,
    refusals: u32,
    retry_at: Option<Instant>,
    problem: Option<String>,
}

impl Events {
    /// Keeps a subscription open, as far as the core lets it.
    pub fn follow(&mut self, socket: &Path, wake: &mpsc::Sender<()>) {
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
                return;
            }
            let refusal = health.refusal.lock().expect("not poisoned").take();
            if refusal.is_none() && steady {
                // the core went away after a while (a restart): subscribe again at once
                self.refusals = 0;
            } else {
                self.refusals += 1;
                let pause = Duration::from_secs(1 << (self.refusals - 1).min(6)).min(MAX_BACKOFF);
                self.retry_at = Some(Instant::now() + pause);
                self.problem = Some(match refusal {
                    Some(refusal) => format!("herdr-core refused the event subscription: {refusal}"),
                    None => "herdr-core closed the event subscription".to_string(),
                });
            }
        }
        // the old subscription is closed before a new one is opened
        self.current = None;
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        self.retry_at = None;
        match Subscription::open(socket, wake.clone()) {
            Ok(subscription) => self.current = Some(subscription),
            Err(err) => self.problem = Some(format!("no herdr-core events: {err:#}")),
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

/// The open `events.subscribe` connection; an event about one of this machine's terminals wakes the collector, and so
/// does its end. Dropping it closes the connection, which ends its reading thread.
struct Subscription {
    opened: Instant,
    stream: UnixStream,
    health: Arc<Health>,
}

/// What the reading thread knows about its subscription.
#[derive(Default)]
struct Health {
    alive: AtomicBool,
    /// The core confirmed the subscription (sent its snapshot).
    started: AtomicBool,
    /// The core's answer when it refused the subscription.
    refusal: Mutex<Option<String>>,
}

impl Subscription {
    fn open(socket: &Path, wake: mpsc::Sender<()>) -> anyhow::Result<Subscription> {
        let mut stream = UnixStream::connect(socket).with_context(|| NoCore(socket.to_path_buf()))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let request =
            json!({ "jsonrpc": "2.0", "id": "herdr-kharka-events", "method": "events.subscribe", "params": {} });
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
                    *seen.refusal.lock().expect("not poisoned") = Some(error_text(error));
                    break;
                }
                if message.get("result").is_some() {
                    seen.started.store(true, Ordering::SeqCst);
                    continue;
                }
                // events about other machines' terminals change nothing written here
                let local = message["params"]["terminal"].as_str().is_none_or(is_local);
                if message["method"] == "event" && local && wake.send(()).is_err() {
                    break;
                }
            }
            // ended by the core, not dropped: the collector looks at once and subscribes again
            if seen.alive.swap(false, Ordering::SeqCst) {
                let _ = wake.send(());
            }
        });
        Ok(Subscription {
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

    #[test]
    fn spaces_are_named_by_name_then_by_where_then_by_folder() {
        let layout = json!({ "spaces": [
            { "id": "sp1", "name": "Review #12", "folder": "/x/repo" },
            { "id": "sp2", "name": "", "folder": "/x/bilendo2" },
            { "id": "sp3", "name": "", "folder": "/x/DMS" },
        ] });
        let terminal = json!({ "labels": { "herdr.space": "sp2", "where": "bilendo / 1" } });
        let spaces = workspaces(&layout, &[&terminal]);
        let labels: Vec<&str> = spaces.iter().map(|space| space["label"].as_str().unwrap()).collect();
        assert_eq!(labels, ["Review #12", "bilendo", "DMS"]);
        assert_eq!(spaces[2]["number"], 3);
    }

    #[test]
    fn only_terminals_that_report_a_status_are_agents() {
        let mut transcripts = Transcripts::default();
        let shell = json!({ "address": "t3", "status": null });
        assert!(agent(&shell, &mut transcripts).is_none());
        let claude = json!({
            "address": "t1", "name": null, "title": "Fix the export", "status_ms": 5,
            "status": { "app": "claude-code", "state": "thinking" },
            "cwd": "/nowhere", "foreground": { "cwd": "/nowhere/deeper", "pid": null },
            "labels": { "herdr.space": "sp1", "herdr.tab": "tb1" }
        });
        let agent = agent(&claude, &mut transcripts).unwrap();
        assert_eq!(agent["pane"], "t1");
        assert_eq!(agent["status"], "unknown");
        assert_eq!(agent["agent"], "claude-code");
        assert_eq!(agent["workspace"], "sp1");
        assert_eq!(agent["cwd"], "/nowhere/deeper");
        assert!(is_local("t1b") && !is_local("omarchy/t3") && !is_local(""));
    }
}
