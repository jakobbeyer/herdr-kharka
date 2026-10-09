//! The local kharka daemon, spoken to through its socket protocol: JSON lines, every answer repeating the request's id.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};

/// The longest wait for the daemon: a daemon that hangs must not hold up the collector.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The daemon answered, but refused the request (the key is not this machine's, the value does not fit the schema): the
/// connection is fine, sending the same again soon would be refused again.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refused: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// Where kharka's daemon listens, looked for as kharka's own client does: `$XDG_RUNTIME_DIR/kharka/daemon.sock`,
/// `/run/user/<uid>/kharka/daemon.sock` (a systemd service's, also when an ssh session has no `$XDG_RUNTIME_DIR`), and
/// `~/.local/state/kharka/daemon.sock`; the first that exists, else the first.
pub fn default_socket() -> PathBuf {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let home = var("HOME").unwrap_or_default();
    let mut candidates = Vec::new();
    if let Some(runtime) = var("XDG_RUNTIME_DIR") {
        candidates.push(runtime.join("kharka/daemon.sock"));
    }
    if let Ok(meta) = std::fs::metadata(&home) {
        use std::os::unix::fs::MetadataExt;
        candidates.push(PathBuf::from(format!("/run/user/{}/kharka/daemon.sock", meta.uid())));
    }
    candidates.push(
        var("XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local/state"))
            .join("kharka/daemon.sock"),
    );
    let first = candidates[0].clone();
    candidates.into_iter().find(|path| path.exists()).unwrap_or(first)
}

pub struct Kharka {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl Kharka {
    pub fn connect(socket: &Path) -> anyhow::Result<Kharka> {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("no kharka daemon at {} (is `kharka daemon` running?)", socket.display()))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        Ok(Kharka {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_id: 1,
        })
    }

    /// Sends a request; its answer when it is ok, else the daemon's error ([`Refused`]).
    fn call(&mut self, mut request: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        request["id"] = json!(id);
        writeln!(self.writer, "{request}").context("writing to the kharka daemon")?;
        loop {
            let mut line = String::new();
            if self
                .reader
                .read_line(&mut line)
                .context("reading from the kharka daemon")?
                == 0
            {
                bail!("the kharka daemon closed the connection");
            }
            let answer: Value =
                serde_json::from_str(&line).context("the kharka daemon sent something that is not JSON")?;
            if answer["id"] != id {
                continue;
            }
            if answer["ok"] != true {
                return Err(Refused(answer["error"].as_str().unwrap_or("no reason given").to_string()).into());
            }
            return Ok(answer);
        }
    }

    /// This machine's node name.
    pub fn whoami(&mut self) -> anyhow::Result<String> {
        let answer = self.call(json!({ "op": "whoami" }))?;
        answer["node"]
            .as_str()
            .map(str::to_string)
            .context("the kharka daemon did not say its name")
    }

    pub fn put(&mut self, key: &str, value: &Value) -> anyhow::Result<()> {
        self.call(json!({ "op": "put", "key": key, "value": value }))
            .with_context(|| format!("writing {key}"))?;
        Ok(())
    }
}
