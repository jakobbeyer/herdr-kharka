//! Commands for this machine's herdr, from any machine. A program writes the record `herdr.commands/<node>/<id>`
//! (`{ from, at, expires, method, params }`); herdr-kharka on that node runs the method against its herdr and writes the
//! answer into the same record (`done: { at, ok, result | error }`); the program reads it and deletes the record.
//!
//! Every `agent.*` method runs, nothing else. A command past its `expires` is answered `expired` without running; an
//! answered one that nobody deleted is deleted after a minute.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};

use crate::kharka::{Kharka, Seen, Watch};
use crate::{Problems, herdr, now_ms};

pub const PREFIX: &str = "herdr.commands/";
/// An answered command still there after this long was left behind by its sender.
const KEEP_ANSWERED_MS: u64 = 60_000;
/// How often the answered commands are looked at when nothing comes.
const TICK: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Follows this node's commands in a thread of its own, for as long as herdr-kharka runs.
pub fn spawn(kharka_socket: PathBuf, herdr_socket: PathBuf) {
    thread::spawn(move || {
        let mut problems = Problems::default();
        let mut pause = Duration::from_millis(250);
        loop {
            if let Err(err) = serve(&kharka_socket, &herdr_socket, &mut problems, &mut pause) {
                problems.report("commands", &format!("{err:#}"));
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
    problems: &mut Problems,
    pause: &mut Duration,
) -> anyhow::Result<()> {
    let mut watch = Watch::open(kharka_socket, &format!("{PREFIX}{{node}}/"))?;
    let mut writer = Kharka::connect(kharka_socket)?;
    problems.clear("commands");
    *pause = Duration::from_millis(250);
    // answered commands: key -> (stamp, answered at)
    let mut answered: HashMap<String, (String, u64)> = HashMap::new();
    loop {
        match watch.next(TICK)? {
            Seen::Change(entry) => take(&entry, herdr_socket, &mut writer, &mut answered)?,
            Seen::Quiet => sweep(&mut writer, &mut answered)?,
            Seen::Ready | Seen::Reset => {}
        }
    }
}

/// A command that came, changed or went: an open one is run and answered, an answered one remembered for the sweep.
fn take(
    entry: &Value,
    herdr_socket: &Path,
    writer: &mut Kharka,
    answered: &mut HashMap<String, (String, u64)>,
) -> anyhow::Result<()> {
    let (Some(key), Some(hlc)) = (entry["key"].as_str(), entry["hlc"].as_str()) else {
        return Ok(());
    };
    let value = &entry["value"];
    if entry["deleted"] == true || !value.is_object() {
        answered.remove(key);
        return Ok(());
    }
    if value["done"].is_object() {
        let at = value["done"]["at"].as_u64().unwrap_or_else(now_ms);
        answered.insert(key.to_string(), (hlc.to_string(), at));
        return Ok(());
    }
    let method = value["method"].as_str().unwrap_or_default();
    let outcome = if method.is_empty() {
        Err("no method".to_string())
    } else if value["expires"].as_u64().is_some_and(|expires| now_ms() > expires) {
        Err("expired before it came".to_string())
    } else if !method.starts_with("agent.") {
        Err(format!("{method}: not allowed, only agent.* methods"))
    } else {
        let params = if value["params"].is_object() {
            value["params"].clone()
        } else {
            json!({})
        };
        herdr::call(herdr_socket, method, &params).map_err(|err| format!("{err:#}"))
    };
    let done = match &outcome {
        Ok(result) => json!({ "at": now_ms(), "ok": true, "result": result }),
        Err(error) => json!({ "at": now_ms(), "ok": false, "error": error }),
    };
    let mut answer = value.clone();
    answer["done"] = done;
    let written = writer.put_if(key, &answer, hlc).context("answering a command")?;
    let from = value["from"].as_str().unwrap_or("?");
    match (&outcome, written) {
        (_, false) => eprintln!("herdr-kharka: command {method} from {from} was withdrawn"),
        (Ok(_), true) => eprintln!("herdr-kharka: command {method} from {from}"),
        (Err(error), true) => eprintln!("herdr-kharka: command {method} from {from} failed: {error}"),
    }
    Ok(())
}

/// Deletes the answers their senders left behind.
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
