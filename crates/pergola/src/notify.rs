//! Desktop feedback for a person who did not start a terminal.
//!
//! A hotkey or the palette runs `pergola <command>` as a detached child, so its
//! stderr goes to the journal — the one place nobody looks. A *rejected* setting
//! is the moment the person needs to know, so it is also sent as a desktop
//! notification.
//!
//! `PERGOLA_NOTIFY` overrides the notifier program (its arguments are kept, the
//! summary and body are appended). That is how the e2e records the call with no
//! notification daemon, and how a desktop can use `dunstify` or `makoctl`.
//! Without `notify-send` and without an override, the failure stays on stderr:
//! a missing notifier must never turn a rejected value into a crash.

use std::process::{Command, Stdio};

/// How to invoke the notifier: the program, its fixed arguments, and the words
/// to append. `None` when `PERGOLA_NOTIFY` is set to nothing useful.
fn invocation(override_command: Option<&str>, message: &str) -> Option<(String, Vec<String>)> {
    let (program, mut args) = match override_command.map(str::trim) {
        Some("") | None => ("notify-send".to_string(), Vec::new()),
        Some(command) => {
            let mut words = command.split_whitespace();
            let program = words.next()?.to_string();
            (program, words.map(str::to_string).collect())
        }
    };
    // `notify-send` (and dunstify) take `[OPTIONS] SUMMARY [BODY]`.
    if program.rsplit('/').next() == Some("notify-send") {
        args.push("--app-name=Pergola".to_string());
    }
    args.push("Pergola".to_string());
    args.push(message.to_string());
    Some((program, args))
}

/// Show `message` as a desktop notification. Never blocks, never fails loudly.
pub fn send(message: &str) {
    send_with_timeout(message, None, false);
}

pub fn workspace(message: &str, recovery_hint: Option<&str>) {
    send_with_timeout(message, recovery_hint, recovery_hint.is_none());
}

fn send_with_timeout(message: &str, default_hint: Option<&str>, brief: bool) {
    let override_command = std::env::var("PERGOLA_NOTIFY").ok();
    let custom_command = override_command
        .as_deref()
        .filter(|cmd| !cmd.trim().is_empty());
    let body = if custom_command.is_some() {
        message
    } else {
        default_hint.unwrap_or(message)
    };
    let Some((program, mut args)) = invocation(custom_command, body) else {
        return;
    };
    if brief && custom_command.is_none() {
        args.insert(0, "--expire-time=1000".into());
    }
    // Detached and silent: the notifier's own output is not ours to show, and a
    // CLI that exits before it finishes must not wait for it.
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    // The daemon lives for days and sends a notification per workspace switch, so
    // the child has to be reaped or it stays a zombie until the daemon exits. A
    // thread waiting on it is the cheapest way: it does not block the caller and
    // a short-lived CLI still exits without waiting.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

#[cfg(test)]
mod tests {
    use super::invocation;

    #[test]
    fn the_default_notifier_is_notify_send_with_the_app_name() {
        let (program, args) = invocation(None, "gap must be a number").unwrap();
        assert_eq!(program, "notify-send");
        assert_eq!(args, vec!["--app-name=Pergola", "Pergola", "gap must be a number"]);
    }

    #[test]
    fn an_override_keeps_its_words_and_takes_the_message() {
        // The notifier may be a stub or `dunstify --urgency=critical`; the
        // override's words stay in front of the summary/body.
        let (program, args) = invocation(Some("dunstify --urgency=critical"), "nope").unwrap();
        assert_eq!(program, "dunstify");
        assert_eq!(args, vec!["--urgency=critical", "Pergola", "nope"]);
    }

    #[test]
    fn an_empty_override_falls_back_to_notify_send() {
        let (program, _) = invocation(Some("   "), "x").unwrap();
        assert_eq!(program, "notify-send");
    }

    #[test]
    fn a_notify_send_override_still_gets_the_app_name() {
        let (_, args) = invocation(Some("/usr/bin/notify-send -t 4000"), "x").unwrap();
        assert_eq!(args, vec!["-t", "4000", "--app-name=Pergola", "Pergola", "x"]);
    }
}
