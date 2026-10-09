//! A launchd agent (macOS) or a systemd user unit (Linux) that keeps herdr-kharka running.

use std::fmt::Write;
use std::path::Path;

const LABEL: &str = "dev.kharka.herdr";

pub fn file(exe: &Path, args: &[String]) -> String {
    let mut command = vec![exe.display().to_string()];
    command.extend(args.iter().cloned());
    if cfg!(target_os = "macos") {
        launchd(&command)
    } else {
        systemd(&command)
    }
}

pub fn instructions() -> String {
    if cfg!(target_os = "macos") {
        format!(
            "herdr-kharka service … > ~/Library/LaunchAgents/{LABEL}.plist\n\
             launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/{LABEL}.plist"
        )
    } else {
        "herdr-kharka service … > ~/.config/systemd/user/herdr-kharka.service\n\
         systemctl --user daemon-reload && systemctl --user enable --now herdr-kharka"
            .to_string()
    }
}

fn launchd(command: &[String]) -> String {
    let mut arguments = String::new();
    for part in command {
        let _ = writeln!(arguments, "        <string>{}</string>", xml(part));
    }
    let log = Path::new(&std::env::var_os("HOME").unwrap_or_default()).join("Library/Logs/herdr-kharka.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{arguments}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        log = xml(&log.display().to_string())
    )
}

fn systemd(command: &[String]) -> String {
    let exec: Vec<String> = command.iter().map(|part| systemd_quote(part)).collect();
    format!(
        "[Unit]\n\
         Description=herdr-kharka: herdr's state into kharka\n\
         After=kharka-daemon.service\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         Restart=always\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec.join(" ")
    )
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn systemd_quote(part: &str) -> String {
    if !part.is_empty()
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-:=@+,{}".contains(&b))
    {
        return part.to_string();
    }
    format!(
        "\"{}\"",
        part.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_runs_the_binary_with_its_arguments() {
        let unit = systemd(&["/bin/herdr-kharka".to_string(), "--key".to_string(), "a b".to_string()]);
        assert!(unit.contains("ExecStart=/bin/herdr-kharka --key \"a b\"\n"));
        let plist = launchd(&["/bin/herdr-kharka".to_string(), "--once".to_string()]);
        assert!(plist.contains("<string>/bin/herdr-kharka</string>\n        <string>--once</string>\n    </array>"));
    }
}
