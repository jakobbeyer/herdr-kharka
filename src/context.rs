//! The context a Claude session uses, from its transcript: `<home>/.claude*/projects/*/<session>.jsonl`.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Map, Value};

/// The end of a transcript that is read for the last usage.
const TAIL_BYTES: u64 = 256 * 1024;

/// The Claude session of an agent, when herdr knows its id.
pub fn session_of(agent: &Map<String, Value>) -> Option<String> {
    let session = agent.get("agent_session")?;
    let id = session["value"].as_str()?;
    let valid = session["kind"] == "id" && id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    valid.then(|| id.to_string())
}

/// The transcripts found so far, by session, and what was read from each: a transcript is read again only when its size
/// or modification time changed.
#[derive(Default)]
pub struct Transcripts {
    found: HashMap<String, Transcript>,
}

struct Transcript {
    path: PathBuf,
    /// The size and modification time when it was read, and the context it held then.
    read: Option<(u64, SystemTime, Option<u64>)>,
}

impl Transcripts {
    /// The tokens the session's context holds after its last answer: input plus cache reads and writes.
    pub fn context(&mut self, session: &str) -> Option<u64> {
        if self
            .found
            .get(session)
            .is_none_or(|transcript| !transcript.path.exists())
        {
            let path = find_transcript(&PathBuf::from(std::env::var_os("HOME")?), session)?;
            self.found.insert(session.to_string(), Transcript { path, read: None });
        }
        let transcript = self.found.get_mut(session)?;
        let meta = fs::metadata(&transcript.path).ok()?;
        let (size, modified) = (meta.len(), meta.modified().ok()?);
        if let Some((read_size, read_modified, context)) = transcript.read
            && (read_size, read_modified) == (size, modified)
        {
            return context;
        }
        let context = context_tokens(&read_tail(&transcript.path).ok()?);
        transcript.read = Some((size, modified, context));
        context
    }
}

fn find_transcript(home: &Path, session: &str) -> Option<PathBuf> {
    let file = format!("{session}.jsonl");
    for config in fs::read_dir(home).ok()?.flatten() {
        if !config.file_name().to_string_lossy().starts_with(".claude") {
            continue;
        }
        let Ok(projects) = fs::read_dir(config.path().join("projects")) else {
            continue;
        };
        for project in projects.flatten() {
            let candidate = project.path().join(&file);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn read_tail(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The usage of the last main-thread message in a transcript's tail.
fn context_tokens(tail: &str) -> Option<u64> {
    tail.lines()
        .rev()
        .filter(|line| !line.contains("\"isSidechain\":true"))
        .find_map(|line| {
            let usage = &line[line.find("\"usage\":{")?..];
            let tokens = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
                .iter()
                .map(|name| number_after(usage, &format!("\"{name}\":")))
                .sum::<u64>();
            (tokens > 0).then_some(tokens)
        })
}

fn number_after(text: &str, label: &str) -> u64 {
    let Some(at) = text.find(label) else { return 0 };
    let digits: String = text[at + label.len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_from_the_last_main_thread_usage() {
        let tail = [
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":5,"output_tokens":7}}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":99999}}}"#,
            r#"{"type":"user","message":"no usage here"}"#,
        ]
        .join("\n");
        assert_eq!(context_tokens(&tail), Some(1015));
        assert_eq!(context_tokens("nothing"), None);
    }

    #[test]
    fn sessions_with_an_id() {
        let agent = json!({ "agent_session": { "kind": "id", "value": "0b7e2c4a-5d1f-4e8b-9a36-2f4c8d1e7a90" } });
        assert_eq!(
            session_of(agent.as_object().unwrap()).as_deref(),
            Some("0b7e2c4a-5d1f-4e8b-9a36-2f4c8d1e7a90")
        );
        let other = json!({ "agent_session": { "kind": "name", "value": "x" } });
        assert_eq!(session_of(other.as_object().unwrap()), None);
    }

    #[test]
    fn a_transcript_is_read_again_only_when_it_changed() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/-repo");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("abc.jsonl");
        let usage = |tokens: u64| format!("{{\"message\":{{\"usage\":{{\"input_tokens\":{tokens}}}}}}}\n");
        fs::write(&path, usage(111)).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let mut transcripts = Transcripts::default();
        transcripts.found.insert(
            "abc".to_string(),
            Transcript {
                path: path.clone(),
                read: None,
            },
        );
        assert_eq!(transcripts.context("abc"), Some(111));

        // same size and time: the cached context, the file is not read
        fs::write(&path, usage(222)).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(transcripts.context("abc"), Some(111));

        // grown: read again
        fs::write(&path, format!("{}{}", usage(222), usage(3333))).unwrap();
        assert_eq!(transcripts.context("abc"), Some(3333));
    }

    #[test]
    fn transcripts_are_found_in_every_claude_profile() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude-work/projects/-Users-x-repo");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("abc.jsonl"), "").unwrap();
        assert_eq!(find_transcript(home.path(), "abc"), Some(project.join("abc.jsonl")));
        assert_eq!(find_transcript(home.path(), "missing"), None);
    }
}
