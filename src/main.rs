//! herdr-kharka: writes what this machine's herdr runs into kharka, whenever it changes.
//!
//! It looks at herdr when herdr reports an event, and every `--interval` anyway, and writes `herdr/<node>` to the local
//! kharka daemon when the state changed, or every `--heartbeat` so readers can tell it is current. kharka carries it to
//! every other machine. It also runs the `agent.*` commands other machines write for this one (see [`commands`]).

mod commands;
mod context;
mod herdr;
mod kharka;
mod service;

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use context::Transcripts;
use herdr::Events;
use kharka::{Kharka, Refused};

/// herdr is written as offline after this many failed looks in a row, or after failing this long; at once only when its
/// socket is gone. Until then the last good look stands, so that one timeout does not empty the machine for a moment.
const OFFLINE_AFTER_FAILURES: u32 = 3;
const OFFLINE_AFTER: Duration = Duration::from_secs(10);

/// Writes what this machine's herdr runs into kharka (`herdr/<node>`), on every change.
#[derive(Parser)]
#[command(version, about, max_term_width = 110)]
struct Cli {
    /// herdr's socket [default: $HERDR_SOCKET_PATH, else ~/.config/herdr/herdr.sock]
    #[arg(long)]
    herdr_socket: Option<PathBuf>,
    /// The kharka daemon's socket [default: kharka's own default]
    #[arg(long, env = "KHARKA_SOCKET")]
    kharka_socket: Option<PathBuf>,
    /// The key to write; {node} becomes the kharka daemon's node name
    #[arg(long, default_value = "herdr/{node}")]
    key: String,
    /// The machine's name for people and for readers such as bhote [default: the kharka daemon's node name]
    #[arg(long)]
    label: Option<String>,
    /// The longest time between two looks at herdr
    #[arg(long, default_value = "3s", value_parser = duration)]
    interval: Duration,
    /// An unchanged state is written again after this long
    #[arg(long, default_value = "10s", value_parser = duration)]
    heartbeat: Duration,
    /// A pane token to leave out, e.g. an animation that would cause a write every second (repeatable)
    #[arg(long = "ignore-token", default_value = "spin")]
    ignore_tokens: Vec<String>,
    /// Look once, write, and end
    #[arg(long)]
    once: bool,
    /// Do not run the agent.* commands that other machines write to herdr.commands/<node>/
    #[arg(long)]
    no_commands: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print a launchd agent (macOS) or systemd user unit (Linux) that keeps herdr-kharka running with these arguments
    Service {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `500ms`, `3s`, `5m`, `1h`.
fn duration(text: &str) -> Result<Duration, String> {
    let digits = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("not a duration: {text:?} (e.g. 3s)"))?;
    let ms = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return Err(format!("not a duration: {text:?} (units: ms, s, m, h)")),
    };
    Ok(Duration::from_millis(number.saturating_mul(ms)))
}

fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli) {
        eprintln!("herdr-kharka: {err:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    if let Some(Command::Service { args }) = cli.command {
        let exe = std::env::current_exe().context("finding the herdr-kharka binary")?;
        print!("{}", service::file(&exe, &args));
        eprintln!("Install it:\n{}", service::instructions());
        return Ok(());
    }
    let herdr_socket = cli.herdr_socket.unwrap_or_else(herdr::default_socket);
    let kharka_socket = cli.kharka_socket.unwrap_or_else(kharka::default_socket);
    if !cli.once && !cli.no_commands {
        commands::spawn(kharka_socket.clone(), herdr_socket.clone());
    }
    let mut herdr = Watcher::new(herdr_socket, cli.ignore_tokens);
    let mut collector = Collector {
        kharka_socket,
        key_pattern: cli.key,
        label: cli.label,
        kharka: None,
        logged_key: None,
        written: None,
        refused: None,
        problems: Problems::default(),
    };
    if cli.once {
        let state = herdr.look(&mut Problems::default());
        return collector.write(&state);
    }
    let (wake, woken) = mpsc::channel::<()>();
    let mut events = Events::default();
    loop {
        let state = herdr.look(&mut collector.problems);
        events.follow(&herdr.socket, &herdr.panes, &wake);
        match events.problem() {
            Some(problem) => collector
                .problems
                .report("herdr events", &format!("{problem} (looking every {:?})", cli.interval)),
            None => collector.problems.clear("herdr events"),
        }
        collector.offer(&state, cli.heartbeat);
        // an event, the interval, or a subscription that is due again; events come in bursts, so wait a moment and
        // take them all
        let wait = events.retry_at().map_or(cli.interval, |at| {
            at.saturating_duration_since(Instant::now()).min(cli.interval)
        });
        if woken.recv_timeout(wait).is_ok() {
            thread::sleep(Duration::from_millis(50));
            while woken.try_recv().is_ok() {}
        }
    }
}

/// herdr as the collector sees it, look by look.
struct Watcher {
    socket: PathBuf,
    ignore_tokens: Vec<String>,
    transcripts: Transcripts,
    /// The state of the last good look; kept through a short failure, dropped when herdr is written as offline.
    last: Option<Value>,
    /// The agent panes of the last good look, whose events are followed.
    panes: Vec<String>,
    failures: u32,
    failing_since: Option<Instant>,
    /// The focused pane of the last good look; `None` before the first.
    focused: Option<Option<String>>,
    /// When a look last found another focused pane than the look before (ms since the epoch).
    focus_changed: Option<u64>,
}

impl Watcher {
    fn new(socket: PathBuf, ignore_tokens: Vec<String>) -> Watcher {
        Watcher {
            socket,
            ignore_tokens,
            transcripts: Transcripts::default(),
            last: None,
            panes: Vec::new(),
            failures: 0,
            failing_since: None,
            focused: None,
            focus_changed: None,
        }
    }

    /// The state to write.
    fn look(&mut self, problems: &mut Problems) -> Value {
        let host = gethostname::gethostname().to_string_lossy().into_owned();
        match herdr::look(&self.socket, &self.ignore_tokens, &mut self.transcripts) {
            Ok(look) => {
                problems.clear("herdr");
                self.failures = 0;
                self.failing_since = None;
                if self.focused.as_ref().is_some_and(|focused| *focused != look.focused) {
                    self.focus_changed = Some(now_ms());
                }
                self.focused = Some(look.focused.clone());
                self.panes = look.panes;
                let state = json!({
                    "host": host,
                    "herdr": "online",
                    "origin_machine_id": look.origin_machine_id,
                    "focused": look.focused,
                    "focus_changed": self.focus_changed,
                    "workspaces": look.workspaces,
                    "agents": look.agents,
                });
                self.last = Some(state.clone());
                state
            }
            Err(err) => {
                problems.report("herdr", &format!("{err:#}"));
                self.failures += 1;
                let failing_since = *self.failing_since.get_or_insert_with(Instant::now);
                let gone = !self.socket.exists()
                    || self.failures >= OFFLINE_AFTER_FAILURES
                    || failing_since.elapsed() >= OFFLINE_AFTER;
                match &self.last {
                    Some(last) if !gone => last.clone(),
                    _ => {
                        self.last = None;
                        json!({
                            "host": host,
                            "herdr": "offline",
                            "origin_machine_id": null,
                            "focused": null,
                            "focus_changed": self.focus_changed,
                            "workspaces": [],
                            "agents": [],
                        })
                    }
                }
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

struct Collector {
    kharka_socket: PathBuf,
    key_pattern: String,
    label: Option<String>,
    /// The connection, the key and the label, once the daemon said its node name.
    kharka: Option<(Kharka, String, String)>,
    /// The key last announced in the log: a new connection to the same key is not announced again.
    logged_key: Option<String>,
    written: Option<(Value, Instant)>,
    /// When the daemon last refused a write.
    refused: Option<Instant>,
    problems: Problems,
}

impl Collector {
    /// Writes the state when it changed, or when the heartbeat is due. A refused write keeps the connection and is
    /// tried again after the heartbeat; a broken connection is opened again on the next round.
    fn offer(&mut self, state: &Value, heartbeat: Duration) {
        let due = self
            .written
            .as_ref()
            .is_none_or(|(value, at)| value != state || at.elapsed() >= heartbeat);
        let held = self.refused.is_some_and(|at| at.elapsed() < heartbeat);
        if !due || held {
            return;
        }
        if let Err(err) = self.write(state) {
            if err.downcast_ref::<Refused>().is_some() {
                self.refused = Some(Instant::now());
            } else {
                self.kharka = None;
            }
            self.problems.report("kharka", &format!("{err:#}"));
        }
    }

    fn write(&mut self, state: &Value) -> anyhow::Result<()> {
        if self.kharka.is_none() {
            let mut kharka = Kharka::connect(&self.kharka_socket)?;
            let node = kharka.whoami()?;
            let key = self.key_pattern.replace("{node}", &node);
            if key.contains('{') {
                bail!("--key {:?}: only {{node}} can be filled in", self.key_pattern);
            }
            if self.logged_key.as_ref() != Some(&key) {
                eprintln!("herdr-kharka: writing {key}");
                self.logged_key = Some(key.clone());
            }
            let label = self.label.clone().unwrap_or(node);
            self.kharka = Some((kharka, key, label));
        }
        let (kharka, key, label) = self.kharka.as_mut().expect("connected above");
        let mut value = state.clone();
        value["label"] = json!(label);
        kharka.put(key, &value)?;
        self.refused = None;
        self.problems.clear("kharka");
        self.written = Some((state.clone(), Instant::now()));
        Ok(())
    }
}

/// Problems are reported when they appear or change, and once when they are gone; not on every round.
#[derive(Default)]
struct Problems {
    current: std::collections::HashMap<&'static str, String>,
}

impl Problems {
    fn report(&mut self, source: &'static str, problem: &str) {
        if self.current.get(source).map(String::as_str) != Some(problem) {
            eprintln!("herdr-kharka: {source}: {problem}");
            self.current.insert(source, problem.to_string());
        }
    }

    fn clear(&mut self, source: &'static str) {
        if self.current.remove(source).is_some() {
            eprintln!("herdr-kharka: {source} is back");
        }
    }
}
