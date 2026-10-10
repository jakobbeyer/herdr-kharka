//! herdr-kharka against a fake herdr and a fake kharka daemon, both on Unix sockets in a throw-away folder.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-kharka");

/// How the fake herdr treats a request.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Answer,
    /// `agent.list` fails this many more times, then herdr answers again.
    FailNext(u32),
    FailAlways,
}

/// A herdr that answers `agent.list` and `workspace.list` from `agents`, and keeps every event subscription open so
/// that the test can send events (or close them, as a restarting herdr does).
struct FakeHerdr {
    agents: Arc<Mutex<Value>>,
    subscribers: Arc<Mutex<Vec<UnixStream>>>,
    /// Every `events.subscribe` request's subscriptions, refused ones too.
    subscriptions: Arc<Mutex<Vec<Value>>>,
    mode: Arc<Mutex<Mode>>,
    /// The event types this herdr knows; it refuses a subscription that names another one. None: every type.
    known_events: Arc<Mutex<Option<Vec<String>>>>,
    /// Every other request (method, params), answered with `{}`, or refused when the target is `missing`.
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    socket: PathBuf,
}

impl FakeHerdr {
    fn start(dir: &Path, agents: Value) -> FakeHerdr {
        let socket = dir.join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let herdr = FakeHerdr {
            agents: Arc::new(Mutex::new(agents)),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            subscriptions: Arc::new(Mutex::new(Vec::new())),
            mode: Arc::new(Mutex::new(Mode::Answer)),
            known_events: Arc::new(Mutex::new(None)),
            calls: Arc::new(Mutex::new(Vec::new())),
            socket,
        };
        let (state, subs, requests, mode, known, calls) = (
            herdr.agents.clone(),
            herdr.subscribers.clone(),
            herdr.subscriptions.clone(),
            herdr.mode.clone(),
            herdr.known_events.clone(),
            herdr.calls.clone(),
        );
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let answer = match request["method"].as_str().unwrap() {
                    "agent.list" => {
                        let mut mode = mode.lock().unwrap();
                        match *mode {
                            Mode::Answer => Ok(json!({
                                "type": "agent_list",
                                "origin_machine_id": "machine_local",
                                "agents": state.lock().unwrap().clone()
                            })),
                            Mode::FailNext(times) => {
                                *mode = if times > 1 {
                                    Mode::FailNext(times - 1)
                                } else {
                                    Mode::Answer
                                };
                                Err("busy")
                            }
                            Mode::FailAlways => Err("busy"),
                        }
                    }
                    "workspace.list" => Ok(json!({ "workspaces": [
                        {
                            "workspace_id": "w1", "label": "repo", "number": 1, "focused": false, "pane_count": 2,
                            "tokens": { "tests": "◌ 3/40", "spin": "✻" }
                        },
                        {
                            "workspace_id": "remote1/w5", "label": "api", "number": 1, "focused": true,
                            "machine_id": "remote1", "machine_profile_id": "remote1", "machine_label": "Remote"
                        }
                    ] })),
                    "events.subscribe" => {
                        let subscriptions = request["params"]["subscriptions"].clone();
                        requests.lock().unwrap().push(subscriptions.clone());
                        let unknown = known.lock().unwrap().as_ref().and_then(|known| {
                            subscriptions.as_array().unwrap().iter().find_map(|subscription| {
                                let kind = subscription["type"].as_str().unwrap();
                                (!known.iter().any(|k| k == kind)).then(|| kind.to_string())
                            })
                        });
                        match unknown {
                            Some(kind) => {
                                let mut stream = stream;
                                let _ = writeln!(
                                    stream,
                                    "{}",
                                    json!({ "id": request["id"], "error": {
                                        "code": "invalid_request",
                                        "message": format!("invalid request: unknown variant `{kind}`")
                                    } })
                                );
                                continue;
                            }
                            None => {
                                subs.lock().unwrap().push(stream.try_clone().unwrap());
                                Ok(json!({ "type": "subscription_started" }))
                            }
                        }
                    }
                    other => {
                        calls
                            .lock()
                            .unwrap()
                            .push((other.to_string(), request["params"].clone()));
                        if request["params"]["target"] == "missing" {
                            Err("pane not found")
                        } else {
                            Ok(json!({}))
                        }
                    }
                };
                let mut stream = stream;
                let message = match answer {
                    Ok(result) => json!({ "id": request["id"], "result": result }),
                    Err(message) => json!({ "id": request["id"], "error": { "code": "busy", "message": message } }),
                };
                let _ = writeln!(stream, "{message}");
            }
        });
        herdr
    }

    fn set_agents(&self, agents: Value) {
        *self.agents.lock().unwrap() = agents;
    }

    fn set_mode(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn send_event(&self) {
        for stream in self.subscribers.lock().unwrap().iter_mut() {
            let _ = writeln!(
                stream,
                "{}",
                json!({ "event": "pane.agent_status_changed", "data": {} })
            );
        }
    }

    /// Closes every event subscription, as herdr does when it restarts.
    fn close_subscriptions(&self) {
        for stream in self.subscribers.lock().unwrap().drain(..) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn subscribed(&self) -> bool {
        !self.subscribers.lock().unwrap().is_empty()
    }

    fn wait_subscribed(&self) {
        for _ in 0..300 {
            if self.subscribed() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("herdr-kharka did not subscribe to herdr's events");
    }

    fn subscription_requests(&self) -> Vec<Value> {
        self.subscriptions.lock().unwrap().clone()
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

/// How the fake kharka daemon answers.
#[derive(Clone, Copy, PartialEq)]
enum Daemon {
    Answer,
    /// Every put is refused, as a hub does with a value that does not fit the schema.
    RefusePuts,
    /// Takes requests and never answers.
    Hang,
    /// Answers a watch only after a second.
    SlowWatch,
    /// Refuses every watch, as a strict hub does without the collection.
    RefuseWatch,
}

/// A kharka daemon that knows its name and hands every put and del to the test (a del as a put of null);
/// `whoamis` counts the connections that asked for the name. A watch is answered and kept, with no entries: the test
/// sends them with `change`.
struct FakeKharka {
    socket: PathBuf,
    puts: Receiver<(String, Value)>,
    whoamis: Arc<Mutex<u32>>,
    watches: Arc<Mutex<Vec<(String, Value, UnixStream)>>>,
}

impl FakeKharka {
    fn wait_watched(&self, prefix: &str) {
        for _ in 0..300 {
            if self
                .watches
                .lock()
                .unwrap()
                .iter()
                .any(|(watched, _, _)| watched == prefix)
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("nobody watches {prefix}");
    }

    /// Sends `ready` (every current entry has come) or `reset` (the watch fell behind) to every watch.
    fn event(&self, event: &str) {
        for (_, id, stream) in self.watches.lock().unwrap().iter_mut() {
            let _ = writeln!(stream, "{}", json!({ "id": id, "event": event }));
        }
    }

    /// Sends an entry to every watch whose prefix it starts with.
    fn change(&self, entry: Value) {
        let key = entry["key"].as_str().unwrap().to_string();
        for (prefix, id, stream) in self.watches.lock().unwrap().iter_mut() {
            if key.starts_with(prefix.as_str()) {
                let _ = writeln!(stream, "{}", json!({ "id": id, "event": "change", "entry": entry }));
            }
        }
    }
}

fn fake_kharka(dir: &Path) -> (PathBuf, Receiver<(String, Value)>) {
    let kharka = fake_kharka_with(dir, Daemon::Answer);
    (kharka.socket, kharka.puts)
}

fn fake_kharka_with(dir: &Path, daemon: Daemon) -> FakeKharka {
    let socket = dir.join("kharka.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (puts, received) = mpsc::channel();
    let whoamis = Arc::new(Mutex::new(0));
    let watches = Arc::new(Mutex::new(Vec::new()));
    let counted = whoamis.clone();
    let watching = watches.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let puts = puts.clone();
            let counted = counted.clone();
            let watching = watching.clone();
            thread::spawn(move || {
                let mut writer = stream.try_clone().unwrap();
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    let request: Value = serde_json::from_str(&line).unwrap();
                    if daemon == Daemon::Hang {
                        continue;
                    }
                    let answer = match request["op"].as_str().unwrap() {
                        "whoami" => {
                            *counted.lock().unwrap() += 1;
                            json!({ "id": request["id"], "ok": true, "node": "mac" })
                        }
                        "put" => {
                            let key = request["key"].as_str().unwrap().to_string();
                            let _ = puts.send((key, request["value"].clone()));
                            if daemon == Daemon::RefusePuts {
                                json!({ "id": request["id"], "ok": false, "error": "does not fit the schema" })
                            } else {
                                json!({ "id": request["id"], "ok": true, "entry": {} })
                            }
                        }
                        "del" => {
                            let _ = puts.send((request["key"].as_str().unwrap().to_string(), Value::Null));
                            json!({ "id": request["id"], "ok": true, "entry": null })
                        }
                        "watch" if daemon == Daemon::RefuseWatch => {
                            json!({ "id": request["id"], "ok": false, "error": "no collection for herdr.commands" })
                        }
                        "watch" => {
                            if daemon == Daemon::SlowWatch {
                                thread::sleep(Duration::from_secs(1));
                            }
                            let _ = writeln!(writer, "{}", json!({ "id": request["id"], "ok": true }));
                            let prefix = request["prefix"].as_str().unwrap().to_string();
                            watching
                                .lock()
                                .unwrap()
                                .push((prefix, request["id"].clone(), writer.try_clone().unwrap()));
                            continue;
                        }
                        other => panic!("unexpected {other}"),
                    };
                    let _ = writeln!(writer, "{answer}");
                }
            });
        }
    });
    FakeKharka {
        socket,
        puts: received,
        whoamis,
        watches,
    }
}

/// Every put that arrives within `within`.
fn puts_within(puts: &Receiver<(String, Value)>, within: Duration) -> Vec<Value> {
    let until = Instant::now() + within;
    let mut values = Vec::new();
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match puts.recv_timeout(left) {
            Ok((_, value)) => values.push(value),
            Err(_) => break,
        }
    }
    values
}

fn agent(pane: &str, status: &str) -> Value {
    json!({
        "pane_id": pane,
        "terminal_id": "term_1",
        "display_agent": "C2",
        "origin_machine_id": "machine_local",
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "name": "repo-1",
        "agent": "claude",
        "agent_status": status,
        "status_since_unix_ms": 1791556115452u64,
        "focused": true,
        "revision": 81392,
        "composer": { "state": "unknown" },
        "terminal_title": "✳ Fix the export",
        "terminal_title_stripped": "Fix the export",
        "cwd": "/nowhere",
        "tokens": { "spin": "✻ working", "repo": "github.com/x/y" }
    })
}

fn collector(herdr: &Path, kharka: &Path, extra: &[&str]) -> Command {
    let mut command = Command::new(BIN);
    command
        .arg("--herdr-socket")
        .arg(herdr)
        .arg("--kharka-socket")
        .arg(kharka)
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn once_writes_this_machines_agents_without_the_noise() {
    let dir = tempfile::tempdir().unwrap();
    let federated = json!({
        "pane_id": "other/w1:p1", "machine_profile_id": "other", "machine_id": "other",
        "origin_machine_id": "machine_other", "agent_status": "idle"
    });
    let herdr = FakeHerdr::start(dir.path(), json!([agent("w1:p1", "working"), federated]));
    let (kharka, puts) = fake_kharka(dir.path());

    let status = collector(&herdr.socket, &kharka, &["--once"]).status().unwrap();
    assert!(status.success());
    let (key, value) = puts.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(key, "herdr/mac");
    assert_eq!(value["herdr"], "online");
    assert_eq!(value["label"], "mac", "the label is the node name by default");
    assert_eq!(
        value.get("commands"),
        None,
        "says nothing about commands: that is the service's to say"
    );
    assert_eq!(value["origin_machine_id"], "machine_local");
    assert_eq!(value["focus_changed"], Value::Null, "no change seen in a single look");
    assert_eq!(value["workspaces"][0]["label"], "repo");
    let agents = value["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "only this machine's agents");
    assert_eq!(
        agents[0],
        json!({
            "pane": "w1:p1",
            "terminal_id": "term_1",
            "display_agent": "C2",
            "origin_machine_id": "machine_local",
            "workspace": "w1",
            "tab": "w1:t1",
            "name": "repo-1",
            "agent": "claude",
            "status": "working",
            "since": 1791556115452u64,
            "title": "Fix the export",
            "session": null,
            "context_tokens": null,
            "cwd": "/nowhere",
            "branch": null,
            "tokens": { "repo": "github.com/x/y" }
        })
    );
    assert_eq!(value["focused"], "w1:p1");
    assert_eq!(
        value["workspaces"],
        json!([{ "id": "w1", "number": 1, "label": "repo", "tokens": { "tests": "◌ 3/40" } }]),
        "only this machine's workspaces, their tokens without the ignored spin"
    );
}

#[test]
fn the_label_can_be_given() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let (kharka, puts) = fake_kharka(dir.path());
    let status = collector(&herdr.socket, &kharka, &["--once", "--label", "Work Mac"])
        .status()
        .unwrap();
    assert!(status.success());
    let (key, value) = puts.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(key, "herdr/mac", "the key keeps the node name");
    assert_eq!(value["label"], "Work Mac");
}

#[test]
fn a_herdr_event_brings_the_change_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([agent("w1:p1", "working")]));
    let (kharka, puts) = fake_kharka(dir.path());
    // looking only every minute: the change below can only come through the event
    let _running = Killed(
        collector(&herdr.socket, &kharka, &["--interval", "60s", "--heartbeat", "60s"])
            .spawn()
            .unwrap(),
    );
    let (_, first) = puts.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first["agents"][0]["status"], "working");
    herdr.wait_subscribed();
    let kinds: Vec<Value> = herdr.subscription_requests()[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|subscription| subscription["type"].clone())
        .collect();
    for kind in [
        "workspace.moved",
        "workspace.reordered",
        "workspace.metadata_updated",
        "tab.moved",
    ] {
        assert!(kinds.contains(&json!(kind)), "subscribed to {kind}");
    }
    assert!(!kinds.contains(&json!("pane.updated")), "not to every frame of a title");

    herdr.set_agents(json!([agent("w1:p1", "done")]));
    herdr.send_event();
    let (_, changed) = puts.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(changed["agents"][0]["status"], "done");

    // an event without a change writes nothing
    herdr.send_event();
    assert!(puts.recv_timeout(Duration::from_millis(500)).is_err());
}

#[test]
fn herdr_away_is_written_as_offline() {
    let dir = tempfile::tempdir().unwrap();
    let (kharka, puts) = fake_kharka(dir.path());
    let status = collector(&dir.path().join("missing.sock"), &kharka, &["--once"])
        .status()
        .unwrap();
    assert!(status.success());
    let (_, value) = puts.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(value["herdr"], "offline");
    assert_eq!(value["agents"], json!([]));
}

#[test]
fn without_a_kharka_daemon_once_fails() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let output = collector(&herdr.socket, &dir.path().join("none.sock"), &["--once"])
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no kharka daemon"));
}

#[test]
fn a_status_readers_do_not_know_becomes_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([agent("w1:p1", "thinking")]));
    let (kharka, puts) = fake_kharka(dir.path());
    assert!(
        collector(&herdr.socket, &kharka, &["--once"])
            .status()
            .unwrap()
            .success()
    );
    let (_, value) = puts.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(value["agents"][0]["status"], "unknown");
}

#[test]
fn an_older_herdr_gets_the_events_it_knows() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([agent("w1:p1", "working")]));
    let legacy = [
        "pane.created",
        "pane.closed",
        "pane.exited",
        "pane.moved",
        "pane.focused",
        "pane.agent_detected",
        "pane.agent_status_changed",
        "workspace.renamed",
        "workspace.closed",
    ];
    *herdr.known_events.lock().unwrap() = Some(legacy.iter().map(|kind| kind.to_string()).collect());
    let (kharka, puts) = fake_kharka(dir.path());
    let _running = Killed(
        collector(&herdr.socket, &kharka, &["--interval", "60s", "--heartbeat", "60s"])
            .spawn()
            .unwrap(),
    );
    puts.recv_timeout(Duration::from_secs(5)).unwrap();
    herdr.wait_subscribed();
    assert_eq!(
        herdr.subscription_requests().len(),
        2,
        "refused once, then the events herdr knows"
    );

    herdr.set_agents(json!([agent("w1:p1", "done")]));
    herdr.send_event();
    let (_, changed) = puts.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(changed["agents"][0]["status"], "done");
}

#[test]
fn a_closed_subscription_is_opened_again() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([agent("w1:p1", "working")]));
    let (kharka, puts) = fake_kharka(dir.path());
    let _running = Killed(
        collector(&herdr.socket, &kharka, &["--interval", "60s", "--heartbeat", "60s"])
            .spawn()
            .unwrap(),
    );
    puts.recv_timeout(Duration::from_secs(5)).unwrap();
    herdr.wait_subscribed();

    // herdr restarts: the subscription ends, herdr-kharka notices and subscribes again (after a second: it ended early)
    herdr.close_subscriptions();
    thread::sleep(Duration::from_millis(100));
    herdr.wait_subscribed();
    assert_eq!(herdr.subscription_requests().len(), 2);

    herdr.set_agents(json!([agent("w1:p1", "done")]));
    herdr.send_event();
    let (_, changed) = puts.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        changed["agents"][0]["status"], "done",
        "the new subscription brings events"
    );
}

#[test]
fn a_refused_subscription_is_asked_for_again_with_a_growing_pause() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([agent("w1:p1", "working")]));
    // a herdr that knows none of the events refuses every subscription, also the older one
    *herdr.known_events.lock().unwrap() = Some(Vec::new());
    let (kharka, _puts) = fake_kharka(dir.path());
    let _running = Killed(
        collector(&herdr.socket, &kharka, &["--interval", "100ms", "--heartbeat", "60s"])
            .spawn()
            .unwrap(),
    );
    thread::sleep(Duration::from_millis(3500));
    // looking every 100 ms; asked again after 1 s and 2 s, not on every look: the full set, the older set, and two
    let asked = herdr.subscription_requests().len();
    assert!((3..=5).contains(&asked), "asked {asked} times");
}

#[test]
fn one_failed_look_keeps_the_last_state() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([agent("w1:p1", "working")]));
    let (kharka, puts) = fake_kharka(dir.path());
    // looking only on events, so that each look below is one event
    let _running = Killed(
        collector(&herdr.socket, &kharka, &["--interval", "60s", "--heartbeat", "60s"])
            .spawn()
            .unwrap(),
    );
    puts.recv_timeout(Duration::from_secs(5)).unwrap();
    herdr.wait_subscribed();

    herdr.set_mode(Mode::FailNext(1));
    herdr.send_event();
    assert!(
        puts_within(&puts, Duration::from_millis(500)).is_empty(),
        "one failure writes nothing"
    );

    herdr.set_mode(Mode::FailAlways);
    for _ in 0..2 {
        herdr.send_event();
        thread::sleep(Duration::from_millis(200));
    }
    let written = puts_within(&puts, Duration::from_millis(500));
    assert!(written.iter().all(|value| value["herdr"] == "offline"));
    assert_eq!(written.len(), 1, "offline after three failures in a row");
    assert_eq!(written[0]["agents"], json!([]));
}

#[test]
fn herdr_gone_is_written_as_offline_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([agent("w1:p1", "working")]));
    let (kharka, puts) = fake_kharka(dir.path());
    let _running = Killed(
        collector(&herdr.socket, &kharka, &["--interval", "60s", "--heartbeat", "60s"])
            .spawn()
            .unwrap(),
    );
    puts.recv_timeout(Duration::from_secs(5)).unwrap();
    herdr.wait_subscribed();

    std::fs::remove_file(&herdr.socket).unwrap();
    herdr.send_event();
    let (_, value) = puts.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(value["herdr"], "offline", "no socket: no waiting");
}

#[test]
fn a_change_of_focus_is_stamped() {
    let dir = tempfile::tempdir().unwrap();
    let mut second = agent("w1:p2", "idle");
    second["focused"] = json!(false);
    let herdr = FakeHerdr::start(dir.path(), json!([agent("w1:p1", "working"), second.clone()]));
    let (kharka, puts) = fake_kharka(dir.path());
    let _running = Killed(
        collector(&herdr.socket, &kharka, &["--interval", "60s", "--heartbeat", "60s"])
            .spawn()
            .unwrap(),
    );
    let (_, first) = puts.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first["focused"], "w1:p1");
    assert_eq!(first["focus_changed"], Value::Null);
    herdr.wait_subscribed();

    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut first_agent = agent("w1:p1", "working");
    first_agent["focused"] = json!(false);
    second["focused"] = json!(true);
    herdr.set_agents(json!([first_agent, second]));
    herdr.send_event();
    let (_, changed) = puts.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(changed["focused"], "w1:p2");
    let stamp = changed["focus_changed"].as_u64().expect("a time");
    assert!(stamp >= before && stamp < before + 5000);
}

#[test]
fn a_refused_write_keeps_the_connection_and_waits() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([agent("w1:p1", "working")]));
    let kharka = fake_kharka_with(dir.path(), Daemon::RefusePuts);
    let started = Instant::now();
    let mut running = collector(
        &herdr.socket,
        &kharka.socket,
        &["--interval", "100ms", "--heartbeat", "1s"],
    )
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    // the state changes on every look, so every look would write
    for round in 0..25 {
        let status = if round % 2 == 0 { "working" } else { "idle" };
        herdr.set_agents(json!([agent("w1:p1", status)]));
        thread::sleep(Duration::from_millis(100));
    }
    running.kill().unwrap();
    // once at the start and once per heartbeat; a slow machine takes longer for the rounds, so the bound follows the time
    let most = started.elapsed().as_secs() as usize + 2;
    let output = running.wait_with_output().unwrap();
    let attempts = puts_within(&kharka.puts, Duration::from_millis(10)).len();
    assert!(
        (2..=most).contains(&attempts),
        "tried again after the heartbeat only: {attempts} (at most {most})"
    );
    assert_eq!(*kharka.whoamis.lock().unwrap(), 1, "one connection");
    let log = String::from_utf8_lossy(&output.stderr);
    assert_eq!(log.matches("does not fit the schema").count(), 1, "logged once: {log}");
    assert_eq!(log.matches("herdr-kharka: writing herdr/mac\n").count(), 1, "{log}");
}

#[test]
fn a_hanging_kharka_daemon_does_not_hold_up_the_collector() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Hang);
    let started = Instant::now();
    let output = collector(&herdr.socket, &kharka.socket, &["--once"])
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(started.elapsed() < Duration::from_secs(15), "gave up after the timeout");
}

/// A command for this machine, as another machine writes it; it expires `expires_in_ms` from now.
fn command(id: &str, method: &str, params: Value, expires_in_ms: i64) -> Value {
    command_from("server", id, method, params, expires_in_ms)
}

fn command_from(from: &str, id: &str, method: &str, params: Value, expires_in_ms: i64) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    json!({
        "key": format!("herdr.commands/mac/{id}"),
        "hlc": format!("{now}-0-server"),
        "value": { "from": from, "at": now, "expires": now + expires_in_ms, "method": method, "params": params }
    })
}

/// The answers written to herdr.commands/ within `within`, by key.
fn answers_within(puts: &Receiver<(String, Value)>, within: Duration) -> Vec<(String, Value)> {
    let until = Instant::now() + within;
    let mut answers = Vec::new();
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match puts.recv_timeout(left) {
            Ok((key, value)) if key.starts_with("herdr.commands/") => answers.push((key, value)),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    answers
}

#[test]
fn a_command_runs_against_herdr_and_is_answered() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([agent("w1:p1", "working")]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    let _running = Killed(
        collector(&herdr.socket, &kharka.socket, &["--commands"])
            .spawn()
            .unwrap(),
    );
    kharka.wait_watched("herdr.commands/mac/");
    let sent = command(
        "server-1",
        "agent.send_keys",
        json!({ "target": "w1:p1", "keys": ["esc"] }),
        15000,
    );
    kharka.change(sent.clone());
    let answers = answers_within(&kharka.puts, Duration::from_secs(2));
    assert_eq!(answers.len(), 1, "{answers:?}");
    let (key, answer) = &answers[0];
    assert_eq!(key, "herdr.commands/mac/server-1");
    assert_eq!(answer["method"], "agent.send_keys", "the command stays");
    assert_eq!(answer["done"]["ok"], true);
    assert_eq!(answer["done"]["result"], json!({}));
    assert_eq!(
        herdr.calls(),
        vec![(
            "agent.send_keys".to_string(),
            json!({ "target": "w1:p1", "keys": ["esc"] })
        )]
    );

    // the answer comes back through the watch and is not run again
    let mut echoed = sent.clone();
    echoed["value"] = answer.clone();
    kharka.change(echoed);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(herdr.calls().len(), 1);
}

#[test]
fn herdrs_error_is_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    let _running = Killed(
        collector(&herdr.socket, &kharka.socket, &["--commands"])
            .spawn()
            .unwrap(),
    );
    kharka.wait_watched("herdr.commands/mac/");
    kharka.change(command(
        "server-2",
        "agent.focus",
        json!({ "target": "missing" }),
        15000,
    ));
    let answers = answers_within(&kharka.puts, Duration::from_secs(2));
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers[0].1["done"]["ok"], false);
    assert_eq!(answers[0].1["done"]["error"], "agent.focus: pane not found");
}

#[test]
fn only_allowed_methods_run_and_only_before_they_expire() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    let _running = Killed(
        collector(&herdr.socket, &kharka.socket, &["--commands"])
            .spawn()
            .unwrap(),
    );
    kharka.wait_watched("herdr.commands/mac/");
    kharka.change(command(
        "server-3",
        "pane.send_text",
        json!({ "target": "w1:p1", "text": "rm -rf ~" }),
        15000,
    ));
    kharka.change(command("server-4", "agent.focus", json!({ "target": "w1:p1" }), -1000));
    let mut forever = command("server-5", "agent.focus", json!({ "target": "w1:p1" }), 0);
    forever["value"].as_object_mut().unwrap().remove("expires");
    kharka.change(forever);
    let answers = answers_within(&kharka.puts, Duration::from_secs(2));
    let errors: Vec<&Value> = answers.iter().map(|(_, answer)| &answer["done"]["error"]).collect();
    assert_eq!(
        errors,
        [
            &json!("pane.send_text: not allowed"),
            &json!("expired before it came"),
            &json!("no expires")
        ]
    );
    assert!(herdr.calls().is_empty(), "nothing ran: {:?}", herdr.calls());
}

#[test]
fn a_command_that_comes_again_is_answered_again_not_run_again() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    let _running = Killed(
        collector(&herdr.socket, &kharka.socket, &["--commands"])
            .spawn()
            .unwrap(),
    );
    kharka.wait_watched("herdr.commands/mac/");
    let sent = command(
        "server-6",
        "agent.send_keys",
        json!({ "target": "w1:p1", "keys": ["enter"] }),
        15000,
    );
    kharka.change(sent.clone());
    let first = answers_within(&kharka.puts, Duration::from_secs(1));
    // as when the watch fell behind and brings every current entry again, before the answer showed
    kharka.change(sent);
    let again = answers_within(&kharka.puts, Duration::from_secs(1));
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(again.len(), 1, "{again:?}");
    assert_eq!(again[0].1["done"], first[0].1["done"], "the same answer");
    assert_eq!(herdr.calls().len(), 1, "ran once: {:?}", herdr.calls());
}

#[test]
fn a_refused_answer_does_not_run_the_command_again() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::RefusePuts);
    let mut running = collector(&herdr.socket, &kharka.socket, &["--commands"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    kharka.wait_watched("herdr.commands/mac/");
    let sent = command("server-7", "agent.send_keys", json!({ "target": "w1:p1" }), 15000);
    kharka.change(sent.clone());
    let answers = answers_within(&kharka.puts, Duration::from_secs(1));
    // as when the watch was opened again
    kharka.change(sent);
    let again = answers_within(&kharka.puts, Duration::from_millis(500));
    running.kill().unwrap();
    let output = running.wait_with_output().unwrap();
    // the answer, then a short one instead
    assert_eq!(answers.len(), 2, "{answers:?}");
    assert_eq!(answers[0].1["done"]["ok"], true);
    assert_eq!(
        answers[1].1["done"]["error"],
        "agent.send_keys: its answer was refused: does not fit the schema"
    );
    assert_eq!(kharka.watches.lock().unwrap().len(), 1, "the watch stays");
    assert_eq!(herdr.calls().len(), 1);
    assert!(again.is_empty(), "given up on, not tried again: {again:?}");
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(log.contains("no answer could be written"), "{log}");
    assert!(!log.contains("from \"server\"\n"), "not logged as answered: {log}");
}

#[test]
fn a_command_deleted_while_nobody_watched_is_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    let _running = Killed(
        collector(&herdr.socket, &kharka.socket, &["--commands"])
            .spawn()
            .unwrap(),
    );
    kharka.wait_watched("herdr.commands/mac/");
    kharka.event("ready");
    let sent = command("server-9", "agent.focus", json!({ "target": "w1:p1" }), 15000);
    kharka.change(sent.clone());
    assert_eq!(answers_within(&kharka.puts, Duration::from_millis(500)).len(), 1);

    // the watch fell behind, the command is still there: answered again, not run again
    kharka.event("reset");
    kharka.change(sent.clone());
    kharka.event("ready");
    assert_eq!(answers_within(&kharka.puts, Duration::from_millis(500)).len(), 1);
    assert_eq!(herdr.calls().len(), 1);

    // it fell behind again and the command was deleted meanwhile: forgotten. A deleted command cannot come back with
    // its stamp; it is sent again only to show that it was forgotten.
    kharka.event("reset");
    kharka.event("ready");
    thread::sleep(Duration::from_millis(200));
    kharka.change(sent);
    assert_eq!(answers_within(&kharka.puts, Duration::from_millis(500)).len(), 1);
    assert_eq!(herdr.calls().len(), 2, "{:?}", herdr.calls());
}

#[test]
fn expires_is_a_number_of_ms() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    let _running = Killed(
        collector(&herdr.socket, &kharka.socket, &["--commands"])
            .spawn()
            .unwrap(),
    );
    kharka.wait_watched("herdr.commands/mac/");
    let mut fraction = command("server-10", "agent.focus", json!({ "target": "w1:p1" }), 15000);
    fraction["value"]["expires"] = json!(fraction["value"]["expires"].as_f64().unwrap() + 0.5);
    kharka.change(fraction);
    let mut negative = command("server-11", "agent.focus", json!({ "target": "w1:p1" }), 15000);
    negative["value"]["expires"] = json!(-1);
    kharka.change(negative);
    let mut text = command("server-12", "agent.focus", json!({ "target": "w1:p1" }), 15000);
    text["value"]["expires"] = json!("soon");
    kharka.change(text);
    let answers = answers_within(&kharka.puts, Duration::from_secs(2));
    let done: Vec<&Value> = answers.iter().map(|(_, answer)| &answer["done"]).collect();
    assert_eq!(done.len(), 3, "{done:?}");
    assert_eq!(done[0]["ok"], true, "a fraction is cut off: {done:?}");
    assert_eq!(done[1]["error"], "expires is not a time in ms");
    assert_eq!(done[2]["error"], "expires is not a time in ms");
    assert_eq!(herdr.calls().len(), 1);
}

#[test]
fn an_agent_method_outside_the_allowlist_runs_only_when_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    let running = Killed(
        collector(&herdr.socket, &kharka.socket, &["--commands"])
            .spawn()
            .unwrap(),
    );
    kharka.wait_watched("herdr.commands/mac/");
    let prompt = json!({ "target": "w1:p1", "text": "go on" });
    kharka.change(command("server-14", "agent.prompt", prompt.clone(), 15000));
    kharka.change(command(
        "server-15",
        "agent.start",
        json!({ "pane_id": "w1:p1" }),
        15000,
    ));
    kharka.change(command("server-16", "agent.get", json!({ "target": "w1:p1" }), 15000));
    kharka.change(command("server-18", "agent.wait", json!({ "target": "w1:p1" }), 15000));
    let answers = answers_within(&kharka.puts, Duration::from_secs(2));
    let done: Vec<&Value> = answers.iter().map(|(_, answer)| &answer["done"]).collect();
    assert_eq!(done.len(), 4, "{done:?}");
    assert_eq!(done[0]["error"], "agent.prompt: not allowed");
    assert_eq!(done[1]["error"], "agent.start: not allowed");
    assert_eq!(done[2]["ok"], true, "a method that only reads is allowed: {done:?}");
    assert_eq!(
        done[3]["error"], "agent.wait: not allowed",
        "it waits longer than a call may take"
    );
    assert_eq!(
        herdr.calls(),
        vec![("agent.get".to_string(), json!({ "target": "w1:p1" }))]
    );
    drop(running);

    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    let _running = Killed(
        collector(
            &herdr.socket,
            &kharka.socket,
            &["--commands", "--allow-method", "agent.prompt"],
        )
        .spawn()
        .unwrap(),
    );
    kharka.wait_watched("herdr.commands/mac/");
    kharka.change(command("server-17", "agent.prompt", prompt.clone(), 15000));
    let answers = answers_within(&kharka.puts, Duration::from_secs(2));
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers[0].1["done"]["ok"], true);
    assert_eq!(herdr.calls(), vec![("agent.prompt".to_string(), prompt)]);
}

/// The `commands` of every herdr/mac written within `within`.
fn advertised_within(puts: &Receiver<(String, Value)>, within: Duration) -> Vec<Value> {
    let until = Instant::now() + within;
    let mut written = Vec::new();
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match puts.recv_timeout(left) {
            Ok((key, value)) if key == "herdr/mac" => written.push(value["commands"].clone()),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    written
}

#[test]
fn the_machine_tells_readers_which_commands_it_takes_while_it_takes_them() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::SlowWatch);
    // looking only every minute: the change below comes because the commands woke the collector
    let _running = Killed(
        collector(
            &herdr.socket,
            &kharka.socket,
            &[
                "--commands",
                "--allow-method",
                "agent.prompt",
                "--interval",
                "60s",
                "--heartbeat",
                "60s",
            ],
        )
        .spawn()
        .unwrap(),
    );
    let written = advertised_within(&kharka.puts, Duration::from_secs(3));
    assert_eq!(written.len(), 2, "{written:?}");
    assert_eq!(written[0], json!([]), "not before the watch is up");
    assert_eq!(
        written[1],
        json!([
            "agent.explain",
            "agent.focus",
            "agent.get",
            "agent.list",
            "agent.prompt",
            "agent.read",
            "agent.rename",
            "agent.send_keys"
        ])
    );

    // a daemon that refuses the watch: no commands are taken, none are told
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::RefuseWatch);
    let _running = Killed(
        collector(&herdr.socket, &kharka.socket, &["--commands", "--interval", "100ms"])
            .spawn()
            .unwrap(),
    );
    let written = advertised_within(&kharka.puts, Duration::from_secs(2));
    assert!(!written.is_empty());
    assert!(written.iter().all(|commands| *commands == json!([])), "{written:?}");
}

#[test]
fn only_agent_methods_can_be_allowed_and_only_with_commands() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let (kharka, puts) = fake_kharka(dir.path());
    for (args, refusal) in [
        (
            &["--commands", "--allow-method", "pane.send_text"][..],
            "only agent.* methods can be allowed",
        ),
        (&["--allow-method", "agent.prompt"][..], "--commands"),
    ] {
        let output = collector(&herdr.socket, &kharka, args)
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?} refused");
        let log = String::from_utf8_lossy(&output.stderr);
        assert!(log.contains(refusal), "{log}");
    }
    assert!(
        puts.recv_timeout(Duration::from_millis(300)).is_err(),
        "nothing written"
    );
}

#[test]
fn at_most_sixty_commands_run_in_a_minute_thirty_per_sender() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    let _running = Killed(
        collector(&herdr.socket, &kharka.socket, &["--commands"])
            .spawn()
            .unwrap(),
    );
    kharka.wait_watched("herdr.commands/mac/");
    let focus = || json!({ "target": "w1:p1" });
    // answered without running: they do not count
    kharka.change(command_from("a", "a-n1", "pane.send_text", json!({}), 15000));
    kharka.change(command_from("a", "a-n2", "agent.focus", focus(), -1000));
    for n in 0..31 {
        kharka.change(command_from("a", &format!("a-{n}"), "agent.focus", focus(), 15000));
    }
    for n in 0..30 {
        kharka.change(command_from("b", &format!("b-{n}"), "agent.focus", focus(), 15000));
    }
    kharka.change(command_from("c", "c-0", "agent.focus", focus(), 15000));
    let answers = answers_within(&kharka.puts, Duration::from_secs(3));
    assert_eq!(answers.len(), 64);
    let refused: Vec<&str> = answers
        .iter()
        .filter(|(_, answer)| answer["done"]["error"] == "too many commands, try again later")
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(
        refused,
        ["herdr.commands/mac/a-30", "herdr.commands/mac/c-0"],
        "the 31st of one sender, the 61st of all"
    );
    assert_eq!(herdr.calls().len(), 60);
}

#[test]
fn a_command_that_does_not_reach_herdr_does_not_count() {
    let dir = tempfile::tempdir().unwrap();
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    // herdr is not there yet
    let _running = Killed(
        collector(&dir.path().join("herdr.sock"), &kharka.socket, &["--commands"])
            .spawn()
            .unwrap(),
    );
    kharka.wait_watched("herdr.commands/mac/");
    for n in 0..31 {
        kharka.change(command(
            &format!("server-u{n}"),
            "agent.focus",
            json!({ "target": "w1:p1" }),
            15000,
        ));
    }
    let answers = answers_within(&kharka.puts, Duration::from_secs(2));
    assert_eq!(answers.len(), 31);
    for (_, answer) in &answers {
        let error = answer["done"]["error"].as_str().unwrap();
        assert!(error.starts_with("no herdr at "), "{error}");
    }

    let herdr = FakeHerdr::start(dir.path(), json!([]));
    kharka.change(command(
        "server-u31",
        "agent.focus",
        json!({ "target": "w1:p1" }),
        15000,
    ));
    let answers = answers_within(&kharka.puts, Duration::from_secs(2));
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].1["done"]["ok"], true, "{answers:?}");
    assert_eq!(herdr.calls().len(), 1);
}

#[test]
fn what_a_sender_writes_cannot_forge_a_line_in_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    let mut running = collector(&herdr.socket, &kharka.socket, &["--commands"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    kharka.wait_watched("herdr.commands/mac/");
    kharka.change(command("server-13", "x\nherdr-kharka: forged", json!({}), 15000));
    assert_eq!(answers_within(&kharka.puts, Duration::from_secs(1)).len(), 1);
    running.kill().unwrap();
    let output = running.wait_with_output().unwrap();
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(log.contains("failed"), "{log}");
    assert!(
        !log.lines().any(|line| line.starts_with("herdr-kharka: forged")),
        "{log}"
    );
}

#[test]
fn without_commands_a_command_is_neither_run_nor_answered() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let kharka = fake_kharka_with(dir.path(), Daemon::Answer);
    let mut running = collector(&herdr.socket, &kharka.socket, &[])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(
        !puts_within(&kharka.puts, Duration::from_millis(500)).is_empty(),
        "it runs"
    );
    kharka.change(command("server-8", "agent.focus", json!({ "target": "w1:p1" }), 15000));
    let answers = answers_within(&kharka.puts, Duration::from_secs(1));
    running.kill().unwrap();
    let output = running.wait_with_output().unwrap();
    assert!(answers.is_empty(), "{answers:?}");
    assert!(herdr.calls().is_empty(), "{:?}", herdr.calls());
    assert!(
        kharka.watches.lock().unwrap().is_empty(),
        "nothing watches herdr.commands/"
    );
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(log.contains("commands are off"), "{log}");
}

#[test]
fn the_service_runs_with_the_arguments_after_service() {
    let output = Command::new(BIN)
        .args([
            "service",
            "--commands",
            "--allow-method",
            "agent.prompt",
            "--label",
            "a b",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let file = String::from_utf8(output.stdout).unwrap();
    if cfg!(target_os = "macos") {
        let arguments = "</string>\n        <string>--commands</string>\n        <string>--allow-method</string>\n        <string>agent.prompt</string>\n        <string>--label</string>\n        <string>a b</string>\n    </array>";
        assert!(file.contains(&format!("{BIN}{arguments}")), "{file}");
    } else {
        assert!(
            file.contains(&format!(
                "ExecStart={BIN} --commands --allow-method agent.prompt --label \"a b\"\n"
            )),
            "{file}"
        );
    }

    // arguments that would not start are refused, not written into a service that fails again and again
    for args in [
        &["service", "--commands", "--allow-method", "pane.send_text"][..],
        &["service", "--once", "--commands"][..],
        &["service", "--commands", "--help"][..],
        &["service", "--version"][..],
        &["service", "--once"][..],
        &["service", "service"][..],
    ] {
        let output = Command::new(BIN).args(args).output().unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }

    // an option before `service` would be left out of the service: refused
    let output = Command::new(BIN).args(["--commands", "service"]).output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn no_commands_is_still_taken_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let (kharka, puts) = fake_kharka(dir.path());
    let status = collector(&herdr.socket, &kharka, &["--once", "--no-commands"])
        .status()
        .unwrap();
    assert!(status.success(), "an older service file still starts");
    puts.recv_timeout(Duration::from_secs(5)).unwrap();

    // contradicting itself: refused, before anything is written
    let status = collector(&herdr.socket, &kharka, &["--no-commands", "--commands"])
        .status()
        .unwrap();
    assert!(!status.success());
    assert!(puts.recv_timeout(Duration::from_millis(300)).is_err());
}

#[test]
fn once_does_not_take_commands() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = FakeHerdr::start(dir.path(), json!([]));
    let (kharka, puts) = fake_kharka(dir.path());
    let output = collector(&herdr.socket, &kharka, &["--once", "--commands"])
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success(), "refused, not ignored");
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    assert!(puts.recv_timeout(Duration::from_millis(300)).is_err());
}
