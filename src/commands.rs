//! Commands for this machine's herdr, from any machine. A program writes the record `herdr.commands/<node>/<id>`
//! (`{ from, at, expires, method, params }`); herdr-kharka on that node runs the method against its herdr and writes the
//! answer into the same record (`done: { at, ok, result | error }`); the program reads it and deletes the record.
//!
//! Only the allowed methods run ([`DEFAULT_METHODS`] and the ones given to `--allow-method`), within the limits
//! ([`LIMIT`] and the ones after it). A command without `expires`, or past it, is answered without running; an answered one that nobody deleted is
//! deleted after a minute, as is one whose answer the daemon refused twice. A command runs at most once, also when it
//! comes again before its answer shows (the watch was opened again or fell behind).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{Value, json};

use crate::kharka::{Kharka, Refused, Seen, Socket, Watch};
use crate::{Problems, herdr, now_ms};

pub const PREFIX: &str = "herdr.commands/";
/// The methods that run without `--allow-method`: the ones herdr-deck sends (list, focus, rename, send keys) and herdr's
/// other agent methods that only read, except `agent.wait`, which waits longer than a call may take. Not
/// `agent.prompt`, `agent.start` or `agent.view.*`, and no method a later herdr adds: they run only when named.
pub const DEFAULT_METHODS: [&str; 7] = [
    "agent.explain",
    "agent.focus",
    "agent.get",
    "agent.list",
    "agent.read",
    "agent.rename",
    "agent.send_keys",
];
/// The limits within `LIMIT_WINDOW`, on the calls that reached herdr: so many in total, so many per sender (`from`, which
/// nobody checks: it holds back a sender that runs wild, not one that lies), and so much time herdr spent on them (a
/// call takes up to 3 s, and they run one after another).
pub const LIMIT: usize = 60;
pub const LIMIT_PER_SENDER: usize = 30;
pub const LIMIT_HERDR_TIME: Duration = Duration::from_secs(20);
const LIMIT_WINDOW: Duration = Duration::from_secs(60);
/// An answered command still there after this long was left behind by its sender.
const KEEP_ANSWERED_MS: u64 = 60_000;
/// How often the answered commands are looked at.
const TICK: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// A command run and not yet seen answered or deleted.
struct Run {
    /// The command's stamp: one that comes again with it is the same command, answered again, not run again.
    hlc: String,
    /// The answer (`done`); `None` when the daemon refused it twice and it was given up on.
    done: Option<Value>,
}

/// The commands run and not yet seen answered or deleted, by key. Kept across connections, so that a command that
/// comes again with the same stamp is answered again, not run again.
type Ran = HashMap<String, Run>;

/// A method `--allow-method` takes: `agent.` and one or more names joined by `.`, each a lower case letter followed by
/// lower case letters, digits and `_`.
pub fn method_name(text: &str) -> Result<String, String> {
    let valid = text.strip_prefix("agent.").is_some_and(|name| {
        name.split('.').all(|part| {
            part.starts_with(|c: char| c.is_ascii_lowercase())
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
    });
    if !valid {
        return Err(format!("only agent.* methods can be allowed, not {text:?}"));
    }
    Ok(text.to_string())
}

/// The methods that run: the defaults and the ones given to `--allow-method`, sorted.
pub fn allowed(extra: &[String]) -> Vec<String> {
    let mut methods: Vec<String> = DEFAULT_METHODS
        .iter()
        .map(|method| method.to_string())
        .chain(extra.iter().cloned())
        .collect();
    methods.sort();
    methods.dedup();
    methods
}

/// What may run: the allowed methods, within the limits. Kept across connections.
struct Guard {
    allowed: Vec<String>,
    /// The calls of the last `LIMIT_WINDOW` that reached herdr, oldest first.
    recent: VecDeque<Call>,
}

/// A call that reached herdr.
struct Call {
    /// When it ended.
    at: Instant,
    took: Duration,
    from: String,
}

impl Guard {
    /// Whether `method`, sent by `from`, may run at `now`.
    fn admit(&mut self, method: &str, from: &str, now: Instant) -> Result<(), String> {
        if !self.allowed.iter().any(|allowed| allowed == method) {
            return Err(format!("{method}: not allowed"));
        }
        while self
            .recent
            .front()
            .is_some_and(|call| now.duration_since(call.at) >= LIMIT_WINDOW)
        {
            self.recent.pop_front();
        }
        let took: Duration = self.recent.iter().map(|call| call.took).sum();
        let sent = self.recent.iter().filter(|call| call.from == from).count();
        if self.recent.len() >= LIMIT || sent >= LIMIT_PER_SENDER || took >= LIMIT_HERDR_TIME {
            return Err("too many commands, try again later".to_string());
        }
        Ok(())
    }

    /// Counts a call that reached herdr, ended at `now` after `took`.
    fn count(&mut self, from: &str, took: Duration, now: Instant) {
        self.recent.push_back(Call {
            at: now,
            took,
            from: from.to_string(),
        });
    }
}

/// Whether this node takes commands right now: its watch is up. The collector writes the allowed methods as `commands`
/// only then, and is woken when it changes.
#[derive(Clone)]
pub struct Up {
    flag: Arc<AtomicBool>,
    wake: mpsc::Sender<()>,
}

impl Up {
    pub fn new(wake: mpsc::Sender<()>) -> Up {
        Up {
            flag: Arc::new(AtomicBool::new(false)),
            wake,
        }
    }

    pub fn get(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    fn set(&self, up: bool) {
        if self.flag.swap(up, Ordering::SeqCst) != up {
            let _ = self.wake.send(());
        }
    }
}

/// What became of an answer.
enum Answer {
    Written,
    /// The command changed or went away since it was read: its sender withdrew it.
    Withdrawn,
    /// The daemon refused the answer and the short error that replaced it, for this reason.
    GaveUp(String),
}

/// Follows this node's commands in a thread of its own, for as long as herdr-kharka runs.
pub fn spawn(kharka_socket: Socket, herdr_socket: PathBuf, allowed: Vec<String>, up: Up) {
    thread::spawn(move || {
        let mut problems = Problems::default();
        let mut pause = Duration::from_millis(250);
        let mut ran = Ran::new();
        let mut guard = Guard {
            allowed,
            recent: VecDeque::new(),
        };
        loop {
            if let Err(err) = serve(
                &kharka_socket,
                &herdr_socket,
                &mut ran,
                &mut guard,
                &up,
                &mut problems,
                &mut pause,
            ) {
                up.set(false);
                // it can hold a key or the daemon's reason: escaped, so that it stays one line
                problems.report("commands", &format!("{err:#}").escape_debug().to_string());
            }
            thread::sleep(pause);
            pause = (pause * 2).min(MAX_BACKOFF);
        }
    });
}

/// One connection's worth: watches `herdr.commands/<node>/` until the connection fails.
fn serve(
    kharka_socket: &Socket,
    herdr_socket: &Path,
    ran: &mut Ran,
    guard: &mut Guard,
    up: &Up,
    problems: &mut Problems,
    pause: &mut Duration,
) -> anyhow::Result<()> {
    let kharka_socket = kharka_socket.path();
    let mut watch = Watch::open(&kharka_socket, &format!("{PREFIX}{{node}}/"))?;
    let mut writer = Kharka::connect(&kharka_socket)?;
    up.set(true);
    problems.clear("commands");
    *pause = Duration::from_millis(250);
    // answered commands: key -> (stamp, answered at)
    let mut answered: HashMap<String, (String, u64)> = HashMap::new();
    // the keys of the current entries while they come, until the watch is ready
    let mut current: Option<HashSet<String>> = Some(HashSet::new());
    let mut swept = Instant::now();
    loop {
        // also when commands keep coming
        let wait = TICK.saturating_sub(swept.elapsed());
        if wait.is_zero() {
            sweep(&mut writer, &mut answered)?;
            swept = Instant::now();
            continue;
        }
        match watch.next(wait)? {
            Seen::Change(entry) => {
                if let (Some(current), Some(key)) = (&mut current, entry["key"].as_str()) {
                    current.insert(key.to_string());
                }
                take(&entry, herdr_socket, &mut writer, &mut answered, ran, guard)?;
            }
            // what is not among the current entries was deleted while nobody watched
            Seen::Ready => {
                if let Some(current) = current.take() {
                    ran.retain(|key, _| current.contains(key));
                    answered.retain(|key, _| current.contains(key));
                }
            }
            Seen::Reset => current = Some(HashSet::new()),
            Seen::Quiet => {}
        }
    }
}

/// A command that came, changed or went: an open one is run and answered, an answered one remembered for the sweep.
fn take(
    entry: &Value,
    herdr_socket: &Path,
    writer: &mut Kharka,
    answered: &mut HashMap<String, (String, u64)>,
    ran: &mut Ran,
    guard: &mut Guard,
) -> anyhow::Result<()> {
    let (Some(key), Some(hlc)) = (entry["key"].as_str(), entry["hlc"].as_str()) else {
        return Ok(());
    };
    let value = &entry["value"];
    if entry["deleted"] == true || !value.is_object() {
        answered.remove(key);
        ran.remove(key);
        return Ok(());
    }
    if value["done"].is_object() {
        let at = value["done"]["at"].as_u64().unwrap_or_else(now_ms);
        answered.insert(key.to_string(), (hlc.to_string(), at));
        ran.remove(key);
        return Ok(());
    }
    let method = value["method"].as_str().unwrap_or_default();
    let from = value["from"].as_str().unwrap_or("?");
    let again = ran.get(key).is_some_and(|run| run.hlc == hlc);
    if !again {
        let done = match run(value, method, from, herdr_socket, guard) {
            Ok(result) => json!({ "at": now_ms(), "ok": true, "result": result }),
            Err(error) => json!({ "at": now_ms(), "ok": false, "error": error }),
        };
        // before it is written: a connection that breaks while writing must not run it again
        let done = Some(done);
        ran.insert(
            key.to_string(),
            Run {
                hlc: hlc.to_string(),
                done,
            },
        );
    }
    let kept = ran.get_mut(key).expect("run above");
    let Some(done) = &mut kept.done else {
        // given up on before: not tried again, the sweep deletes it
        answered
            .entry(key.to_string())
            .or_insert_with(|| (hlc.to_string(), now_ms()));
        return Ok(());
    };
    let answer = write_answer(writer, key, hlc, value, done, method)?;
    let error = done["error"].as_str().map(str::to_string);
    if let Answer::GaveUp(_) = answer {
        kept.done = None;
        answered.insert(key.to_string(), (hlc.to_string(), now_ms()));
    }
    // an answer written again was logged the first time
    if !(again && matches!(answer, Answer::Written)) {
        log(method, from, error.as_deref(), &answer);
    }
    Ok(())
}

/// Runs the command: its result, or the error to answer with.
fn run(value: &Value, method: &str, from: &str, herdr_socket: &Path, guard: &mut Guard) -> Result<Value, String> {
    match (method, expires(value)) {
        ("", _) => Err("no method".to_string()),
        (_, Err(error)) => Err(error.to_string()),
        (_, Ok(expires)) if now_ms() > expires => Err("expired before it came".to_string()),
        (method, _) => {
            guard.admit(method, from, Instant::now())?;
            let params = if value["params"].is_object() {
                value["params"].clone()
            } else {
                json!({})
            };
            let started = Instant::now();
            let result = herdr::call(herdr_socket, method, &params);
            // one that did not reach herdr (it restarts) cost herdr nothing
            if !result
                .as_ref()
                .is_err_and(|err| err.downcast_ref::<herdr::NoHerdr>().is_some())
            {
                guard.count(from, started.elapsed(), Instant::now());
            }
            result.map_err(|err| format!("{err:#}"))
        }
    }
}

/// When the command expires (ms): any number not below zero, a fraction cut off.
fn expires(value: &Value) -> Result<u64, &'static str> {
    let expires = &value["expires"];
    if expires.is_null() {
        return Err("no expires");
    }
    expires
        .as_u64()
        .or_else(|| {
            expires
                .as_f64()
                .filter(|ms| ms.is_finite() && *ms >= 0.0)
                .map(|ms| ms as u64)
        })
        .ok_or("expires is not a time in ms")
}

/// One line per answer. What the sender wrote, and herdr's error that may repeat it, is quoted, so that it cannot
/// start a line of its own.
fn log(method: &str, from: &str, error: Option<&str>, answer: &Answer) {
    let command = format!("herdr-kharka: command {method:?} from {from:?}");
    match (answer, error) {
        (Answer::Withdrawn, _) => eprintln!("{command} was withdrawn"),
        (Answer::GaveUp(reason), _) => eprintln!("{command}: no answer could be written: {reason:?}"),
        (Answer::Written, None) => eprintln!("{command}"),
        (Answer::Written, Some(error)) => eprintln!("{command} failed: {error:?}"),
    }
}

/// Writes the command with its answer `done` if the command is unchanged since `hlc`. An answer the daemon refuses
/// (e.g. a result too large for a value) is replaced in `done` by a short error; one refused again is given up on, so
/// that the connection stays and the command does not come again.
fn write_answer(
    writer: &mut Kharka,
    key: &str,
    hlc: &str,
    command: &Value,
    done: &mut Value,
    method: &str,
) -> anyhow::Result<Answer> {
    let mut answer = command.clone();
    answer["done"] = done.clone();
    let reason = match put(writer, key, &answer, hlc)? {
        Ok(written) => return Ok(written),
        Err(reason) => reason,
    };
    *done = json!({ "at": now_ms(), "ok": false, "error": format!("{method}: its answer was refused: {reason}") });
    answer["done"] = done.clone();
    Ok(match put(writer, key, &answer, hlc)? {
        Ok(written) => written,
        Err(reason) => Answer::GaveUp(reason),
    })
}

/// A conditional put: written or withdrawn, or the daemon's reason when it refused.
fn put(writer: &mut Kharka, key: &str, answer: &Value, hlc: &str) -> anyhow::Result<Result<Answer, String>> {
    match writer.put_if(key, answer, hlc) {
        Ok(true) => Ok(Ok(Answer::Written)),
        Ok(false) => Ok(Ok(Answer::Withdrawn)),
        Err(err) => match err.downcast_ref::<Refused>() {
            Some(Refused(reason)) => Ok(Err(reason.clone())),
            None => Err(err.context("answering a command")),
        },
    }
}

/// Deletes the answers their senders left behind, and the commands no answer could be written to.
fn sweep(writer: &mut Kharka, answered: &mut HashMap<String, (String, u64)>) -> anyhow::Result<()> {
    let now = now_ms();
    let old: Vec<(String, String)> = answered
        .iter()
        .filter(|(_, (_, at))| now.saturating_sub(*at) > KEEP_ANSWERED_MS)
        .map(|(key, (hlc, _))| (key.clone(), hlc.clone()))
        .collect();
    for (key, hlc) in old {
        writer.del_if(&key, &hlc).context("deleting an old command")?;
        answered.remove(&key);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> Guard {
        Guard {
            allowed: allowed(&[]),
            recent: VecDeque::new(),
        }
    }

    const RUNS: Result<(), String> = Ok(());

    fn too_many() -> Result<(), String> {
        Err("too many commands, try again later".to_string())
    }

    #[test]
    fn at_most_sixty_calls_in_the_last_minute() {
        let mut guard = guard();
        let start = Instant::now();
        let quick = Duration::from_millis(10);
        for n in 0..LIMIT as u64 {
            let at = start + Duration::from_secs(n / 2);
            let from = if n % 2 == 0 { "a" } else { "b" };
            assert_eq!(guard.admit("agent.list", from, at), RUNS);
            guard.count(from, quick, at);
        }
        let full = start + Duration::from_secs(40);
        assert_eq!(guard.admit("agent.list", "c", full), too_many());
        assert_eq!(
            guard.admit("agent.prompt", "c", full),
            Err("agent.prompt: not allowed".to_string())
        );
        // a minute after the first two ended, two more may run
        let later = start + LIMIT_WINDOW;
        for _ in 0..2 {
            assert_eq!(guard.admit("agent.list", "c", later), RUNS);
            guard.count("c", quick, later);
        }
        assert_eq!(guard.admit("agent.list", "c", later), too_many());
    }

    #[test]
    fn at_most_thirty_calls_per_sender() {
        let mut guard = guard();
        let now = Instant::now();
        for _ in 0..LIMIT_PER_SENDER {
            assert_eq!(guard.admit("agent.list", "deck", now), RUNS);
            guard.count("deck", Duration::from_millis(10), now);
        }
        assert_eq!(guard.admit("agent.list", "deck", now), too_many());
        assert_eq!(guard.admit("agent.list", "other", now), RUNS, "the others still may");
    }

    #[test]
    fn at_most_twenty_seconds_of_herdr_in_the_last_minute() {
        let mut guard = guard();
        let start = Instant::now();
        // slow calls from different senders: far below the counts
        for n in 0..7u64 {
            let at = start + Duration::from_secs(3 * n);
            assert_eq!(guard.admit("agent.read", &format!("s{n}"), at), RUNS);
            guard.count(&format!("s{n}"), Duration::from_millis(2900), at);
        }
        let at = start + Duration::from_secs(21);
        assert_eq!(guard.admit("agent.list", "s9", at), too_many(), "20.3 s of herdr");
        // the first one leaves the window
        assert_eq!(guard.admit("agent.list", "s9", start + LIMIT_WINDOW), RUNS);
    }

    #[test]
    fn only_agent_methods_can_be_allowed() {
        for name in ["agent.prompt", "agent.view.set", "agent.send_keys", "agent.v2"] {
            assert_eq!(method_name(name), Ok(name.to_string()));
        }
        for refused in [
            "pane.run",
            "agent.",
            "agent",
            "agent.Prompt",
            "agent.prompt\n",
            "xagent.list",
            "agent..",
            "agent._",
            "agent.1",
            "agent.view.",
            "agent.view..set",
            "agent.view._x",
        ] {
            assert!(method_name(refused).is_err(), "{refused}");
        }
    }
}
