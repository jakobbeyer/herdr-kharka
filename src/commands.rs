//! Commands for this machine's herdr, from any machine. A program writes the record `herdr.commands/<node>/<id>`
//! (`{ from, at, expires, method, params }`); herdr-kharka on that node runs the method against its herdr and writes the
//! answer into the same record (`done: { at, ok, result | error }`); the program reads it and deletes the record.
//!
//! Every `agent.*` method runs, nothing else. A command without `expires`, or past it, is answered without running; an
//! answered one that nobody deleted is deleted after a minute, as is one whose answer the daemon refused twice. A
//! command runs at most once, also when it comes again before its answer shows (the watch was opened again or fell
//! behind).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{Value, json};

use crate::kharka::{Kharka, Refused, Seen, Watch};
use crate::{Problems, herdr, now_ms};

pub const PREFIX: &str = "herdr.commands/";
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

/// What became of an answer.
enum Answer {
    Written,
    /// The command changed or went away since it was read: its sender withdrew it.
    Withdrawn,
    /// The daemon refused the answer and the short error that replaced it, for this reason.
    GaveUp(String),
}

/// Follows this node's commands in a thread of its own, for as long as herdr-kharka runs.
pub fn spawn(kharka_socket: PathBuf, herdr_socket: PathBuf) {
    thread::spawn(move || {
        let mut problems = Problems::default();
        let mut pause = Duration::from_millis(250);
        let mut ran = Ran::new();
        loop {
            if let Err(err) = serve(&kharka_socket, &herdr_socket, &mut ran, &mut problems, &mut pause) {
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
    kharka_socket: &Path,
    herdr_socket: &Path,
    ran: &mut Ran,
    problems: &mut Problems,
    pause: &mut Duration,
) -> anyhow::Result<()> {
    let mut watch = Watch::open(kharka_socket, &format!("{PREFIX}{{node}}/"))?;
    let mut writer = Kharka::connect(kharka_socket)?;
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
                take(&entry, herdr_socket, &mut writer, &mut answered, ran)?;
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
        let done = match run(value, method, herdr_socket) {
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
fn run(value: &Value, method: &str, herdr_socket: &Path) -> Result<Value, String> {
    match (method, expires(value)) {
        ("", _) => Err("no method".to_string()),
        (_, Err(error)) => Err(error.to_string()),
        (_, Ok(expires)) if now_ms() > expires => Err("expired before it came".to_string()),
        (method, _) if !method.starts_with("agent.") => Err(format!("{method}: not allowed, only agent.* methods")),
        (method, _) => {
            let params = if value["params"].is_object() {
                value["params"].clone()
            } else {
                json!({})
            };
            herdr::call(herdr_socket, method, &params).map_err(|err| format!("{err:#}"))
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
