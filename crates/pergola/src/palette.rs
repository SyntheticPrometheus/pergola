//! A human-facing action palette.
//!
//! The daemon takes its orders from a hotkey or the socket, so a person who has
//! neither in mind would need a terminal to tile a window. `pergola palette`
//! puts the action list behind whatever menu program the desktop already ships,
//! and hands the chosen line back to `pergola <action>`.

use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{anyhow, Context, Result};

/// One row of the palette: the words a human reads and the argv they stand for.
/// The label is owned because a workspace row names a tag the document holds,
/// and a tag name only exists at run time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub argv: Vec<String>,
}

/// What the palette knows about the window it would act on. Read from the
/// daemon so the list is not a fixed advertisement: a row that would do nothing
/// is left out, and the prompt names the window. The default (no daemon, no
/// focused window) keeps every row, because a menu that refuses to open is
/// worse than a row that turns out to be a no-op.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub title: String,
    /// The focused window is on the desktop layer: "float" would do nothing and
    /// folding it into a tile is the action that makes sense.
    pub floating: bool,
    /// The tag names the document actually holds. Empty means the person has
    /// not started a workspace, and the palette stays quiet about them.
    pub tags: Vec<String>,
    /// The tag on screen, so its own row is not offered (there is nowhere to go).
    pub active_tag: Option<String>,
}

const TILE: &[(&str, &[&str])] = &[
    ("Tile this window left", &["dock", "left"]),
    ("Tile this window right", &["dock", "right"]),
    ("Tile this window above", &["dock", "up"]),
    ("Tile this window below", &["dock", "down"]),
];

const RATIOS: &[(&str, &[&str])] = &[
    ("Split this split half and half", &["ratio", "0.5"]),
    ("Give this window one third", &["ratio", "0.33"]),
    ("Give this window two thirds", &["ratio", "0.67"]),
];

const CAMERA: &[(&str, &[&str])] = &[
    ("Arrange all windows", &["arrange"]),
    ("Float all windows", &["float-all"]),
    ("Zoom out one level", &["zoom-out"]),
    ("Frame this window", &["frame-window"]),
    ("Show the whole tree", &["everything"]),
    ("Split with a new terminal", &["split-terminal"]),
    ("Toggle the split overlay", &["overlay", "toggle"]),
];

/// The rows for one state. Order is deliberate: the four dock edges, then the
/// one action that changes where the window lives, then Undo where it remains
/// visible in a short menu, then the ratios and whole-layout commands.
pub fn items_for(state: &State) -> Vec<Item> {
    let mut rows: Vec<Item> = TILE
        .iter()
        .map(|&(label, argv)| row(label, argv))
        .collect();
    if state.floating {
        rows.push(row("Fold this window into a tab", &["tab"]));
    } else {
        rows.push(row("Float this window", &["float"]));
    }
    rows.push(row("Undo the last layout change", &["undo"]));
    rows.extend(RATIOS.iter().map(|&(label, argv)| row(label, argv)));
    rows.extend(CAMERA.iter().map(|&(label, argv)| row(label, argv)));
    rows.extend(workspace_rows(state));
    rows
}

/// A row from a static table, copied into owned strings.
fn row(label: &str, argv: &[&str]) -> Item {
    Item {
        label: label.to_string(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
    }
}

/// Workspace rows, built from the tags the document holds: switching to one,
/// and moving the focused window onto one. A row that would do nothing (the
/// tag already on screen, or no window to move) is left out. No tags, no rows:
/// a desktop that never started a workspace sees no workspace menu.
fn workspace_rows(state: &State) -> Vec<Item> {
    let mut rows = Vec::new();
    if state.active_tag.is_some() {
        rows.push(row(
            "Show windows from all workspaces",
            &["tag-view", "all"],
        ));
    }
    for tag in &state.tags {
        if state.active_tag.as_deref() == Some(tag.as_str()) {
            continue;
        }
        rows.push(row(
            &format!("Switch to workspace {tag}"),
            &["tag-view", tag],
        ));
    }
    if !state.title.is_empty() {
        for tag in &state.tags {
            rows.push(row(
                &format!("Move this window to workspace {tag}"),
                &["window-tag", tag],
            ));
        }
    }
    rows
}

/// The full list, for a caller with no daemon to ask (tests and `--list` with
/// no daemon both go through `items_for`).
#[cfg(test)]
pub fn items() -> Vec<Item> {
    items_for(&State::default())
}

/// The focused window's state, read from the daemon. Two cheap requests: the
/// active window (and its panel) from `status`, then the tree to see whether
/// that panel is tiled or floating and what the window is called.
pub fn state(socket: &Path) -> State {
    let Ok(status) = crate::ipc::call(socket, &["status".to_string()]) else {
        return State::default();
    };
    if !status.ok {
        return State::default();
    }
    // X11 window 0 is None. An empty workspace still needs its navigation rows.
    let (window, panel) = parse_active(&status.message).unwrap_or((0, None));
    let Ok(tree) = crate::ipc::call(socket, &["tree".to_string()]) else {
        return State::default();
    };
    if !tree.ok {
        return State::default();
    }
    state_from_tree(&tree.message, window, panel.as_deref())
}

/// `active 0x3138538 (p-3138538)` in `status`, or `active none`. The panel is
/// absent for a window the daemon does not manage.
fn parse_active(status: &str) -> Option<(u64, Option<String>)> {
    let tail = status.rsplit_once(" · active ")?.1.trim();
    if tail == "none" {
        return None;
    }
    let window = u64::from_str_radix(tail.strip_prefix("0x")?.split_whitespace().next()?, 16).ok()?;
    let panel = tail
        .split_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .filter(|panel| *panel != "unmanaged" && *panel != "floating")
        .map(str::to_string);
    Some((window, panel))
}

/// Whether the panel is in the tree (tiled) or the floating list, plus the
/// window's title from the seat records.
fn state_from_tree(tree: &str, window: u64, panel: Option<&str>) -> State {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(tree) else {
        return State::default();
    };
    let tags: Vec<String> = value
        .pointer("/doc/tags")
        .and_then(|tags| tags.as_array())
        .map(|tags| {
            tags.iter()
                .filter_map(|tag| tag.get("name").and_then(|n| n.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let active_tag = value
        .pointer("/doc/active_tag")
        .and_then(|tag| tag.as_str())
        .map(str::to_string);
    let title = value
        .pointer("/pergola/seats")
        .and_then(|seats| seats.as_array())
        .and_then(|seats| {
            seats.iter().find(|seat| {
                seat.get("window").and_then(|w| w.as_u64()) == Some(window)
            })
        })
        .and_then(|seat| seat.get("title").and_then(|t| t.as_str()))
        .unwrap_or_default()
        .to_string();
    let Some(panel) = panel else {
        return State {
            title,
            floating: false,
            tags,
            active_tag,
        };
    };
    let floating = value
        .pointer("/doc/floating")
        .and_then(|f| f.as_array())
        .map(|list| {
            list.iter().any(|f| {
                f.pointer("/panel/id").and_then(|id| id.as_str()) == Some(panel)
            })
        })
        .unwrap_or(false);
    State {
        title,
        floating,
        tags,
        active_tag,
    }
}

/// Print the rows for the current state, one per line with the argv they stand
/// for, so a script (or the test suite) can see what the menu would offer
/// without a menu program.
pub fn list(socket: &Path) -> Vec<(String, String)> {
    let state = state(socket);
    items_for(&state)
        .into_iter()
        .map(|item| (item.label, item.argv.join(" ")))
        .collect()
}

/// The prompt: `pergola` alone, or the focused window's title when the daemon
/// answered. It still starts with `pergola`, which is what menu wrapper scripts
/// key on.
fn prompt(state: &State) -> String {
    if state.title.is_empty() {
        "pergola".to_string()
    } else {
        format!("pergola · {}", state.title)
    }
}

/// Show the palette in a detached `pergola palette-run` process.
///
/// The caller returns immediately: a hotkey is handled inside the daemon's
/// event loop, and the CLI exits as soon as it has printed. The menu itself
/// therefore must live in a process of its own, not a thread of either.
pub fn open(socket: &Path) -> Result<()> {
    if find_menu().is_none() {
        return Err(missing_menu());
    }
    let exe = env::current_exe().context("cannot locate the pergola executable")?;
    let mut child = Command::new(exe)
        .arg("--socket")
        .arg(socket)
        .arg("palette-run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| "cannot start the palette process")?;
    // The daemon lives for days: reap the helper so it cannot become a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Run the menu and hand the chosen action to `pergola <action>`. Blocking:
/// this is what `pergola palette-run` does in its own process.
pub fn run(socket: &Path) -> Result<()> {
    let menu = find_menu().ok_or_else(|| {
        let error = missing_menu();
        notify_failure(&format!("{error:#}"));
        error
    })?;
    run_menu(&menu, socket)
}

/// Show the settings palette in a detached `pergola settings-run` process, so a
/// hotkey does not block the daemon's event loop while a menu is open.
pub fn open_settings(socket: &Path) -> Result<()> {
    if find_menu().is_none() {
        return Err(missing_menu());
    }
    let exe = env::current_exe().context("cannot locate the pergola executable")?;
    let mut child = Command::new(exe)
        .arg("--socket")
        .arg(socket)
        .arg("settings-run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| "cannot start the settings process")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// One pass of the settings menu: pick a row, type a value, apply it. Blocking,
/// so it runs in its own process (`pergola settings-run`).
pub fn run_settings(socket: &Path) -> Result<()> {
    let menu = find_menu().ok_or_else(|| {
        let error = missing_menu();
        notify_failure(&format!("{error:#}"));
        error
    })?;
    let report = crate::config::Config::inspect();
    let rows = crate::config::settings_rows(&report.config);
    let labels: Vec<&str> = rows.iter().map(|(label, _)| label.as_str()).collect();
    let output = visible_menu_error(show_list(&menu, "pergola settings", &labels))?;
    let Some(label) = checked_choice(&output, menu.kind, "settings menu")? else {
        return Ok(()); // cancelled
    };
    let Some((_, setting)) = rows.iter().find(|(row, _)| row == &label) else {
        let message = format!("settings menu returned an unknown choice: {label:?}");
        notify_failure(&message);
        anyhow::bail!(message);
    };
    let (key, current) = match setting {
        crate::config::Setting::Key(key) => (key.to_string(), crate::config::value_text(&report.config, key)),
        crate::config::Setting::Chord(command) => {
            let current = pergola_x11::binding_plan_with(&report.config.bindings, report.config.tags)
                .into_iter()
                .find(|(name, _)| name == command)
                .map(|(_, chords)| chords.join(", "))
                .unwrap_or_default();
            (command.clone(), current)
        }
    };
    let typed = visible_menu_error(entry(&menu, &key, &current))?;
    let Some(value) = checked_choice(&typed, menu.kind, "settings entry")? else {
        return Ok(()); // cancelled
    };
    let argv: Vec<String> = match setting {
        crate::config::Setting::Key(key) => vec!["settings".into(), key.to_string(), value],
        // A chord row is a `bind`: it already validates, writes and reloads.
        // The entry prefills `Super+a, Super+b`, and `bind` takes one chord per
        // argument, so the commas are the separator here — the row accepts what
        // it shows instead of failing to parse its own prefill.
        crate::config::Setting::Chord(command) => {
            let mut argv = vec!["bind".to_string(), command.clone()];
            argv.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|chord| !chord.is_empty())
                    .map(str::to_string),
            );
            argv
        }
    };
    spawn_argv(socket, &argv)
}

/// Ask for one line of text, prefilled with the value in force where the menu
/// program can do that.
fn entry(menu: &Menu, prompt: &str, current: &str) -> Result<Output> {
    match menu.kind {
        Kind::Stdin => Command::new(&menu.path)
            .args(["-dmenu", "-p", prompt, "-filter", current])
            .output()
            .with_context(|| format!("cannot run {}", menu.path.display())),
        Kind::Dmenu => run_stdin_menu(&menu.path, &["-p", prompt], &[current]),
        Kind::Zenity => Command::new(&menu.path)
            .args(["--entry", &format!("--text={prompt}"), &format!("--entry-text={current}")])
            .output()
            .with_context(|| format!("cannot run {}", menu.path.display())),
        Kind::Yad => Command::new(&menu.path)
            .args(["--entry", &format!("--text={prompt}"), &format!("--entry-text={current}")])
            .output()
            .with_context(|| format!("cannot run {}", menu.path.display())),
    }
}

/// The list half of a menu run, shared by the action palette and the settings.
fn show_list(menu: &Menu, prompt: &str, labels: &[&str]) -> Result<Output> {
    match menu.kind {
        Kind::Stdin => run_stdin_menu(&menu.path, &["-dmenu", "-i", "-p", prompt], labels),
        Kind::Dmenu => run_stdin_menu(&menu.path, &["-i", "-p", prompt], labels),
        Kind::Zenity => {
            let title = format!("--title={prompt}");
            run_list_menu(&menu.path, &["--list", title.as_str()], labels)
        }
        Kind::Yad => run_list_menu(&menu.path, &["--list", "--no-headers"], labels),
    }
}

fn spawn_argv(socket: &Path, argv: &[String]) -> Result<()> {
    let exe = env::current_exe().context("cannot locate the pergola executable")?;
    let status = Command::new(exe)
        .arg("--socket")
        .arg(socket)
        .args(argv)
        .status()
        .with_context(|| format!("cannot run `pergola {}`", argv.join(" ")))?;
    if !status.success() {
        let message = format!("settings: `pergola {}` failed ({status})", argv.join(" "));
        // Scalar settings validation already notifies in the child. A failed
        // chord bind has no other desktop feedback.
        if argv.first().map(String::as_str) != Some("settings") {
            notify_failure(&message);
        }
        anyhow::bail!(message);
    }
    Ok(())
}

/// Name the actual problem: a broken `PERGOLA_MENU` is not "no menu program".
fn missing_menu() -> anyhow::Error {
    let custom = env::var("PERGOLA_MENU").unwrap_or_default();
    let custom = custom.trim();
    if custom.is_empty() {
        anyhow!("no menu program found — install rofi (recommended) or set PERGOLA_MENU")
    } else {
        anyhow!("PERGOLA_MENU={custom} is not an executable menu — fix it or unset it")
    }
}

/// How a menu program wants its choices: on stdin, or as list columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Stdin,
    Dmenu,
    Zenity,
    Yad,
}

#[derive(Debug)]
struct Menu {
    path: PathBuf,
    kind: Kind,
}

fn find_menu() -> Option<Menu> {
    let custom = env::var("PERGOLA_MENU").unwrap_or_default();
    let custom = custom.trim();
    if !custom.is_empty() {
        // A typo must fail loudly at `pergola palette`, not silently inside the
        // detached helper where nobody reads stderr.
        let path = resolve(custom)?;
        let kind = kind_of(Path::new(custom));
        return Some(Menu { path, kind });
    }
    ["rofi", "zenity", "yad", "dmenu"]
        .into_iter()
        .find_map(|name| {
            let path = on_path(name)?;
            Some(Menu {
                path,
                kind: kind_of(Path::new(name)),
            })
        })
}

/// A custom `PERGOLA_MENU` speaks the protocol of whichever menu it names.
fn kind_of(command: &Path) -> Kind {
    let name = command.file_name().unwrap_or_default().to_string_lossy();
    if name.contains("zenity") {
        Kind::Zenity
    } else if name.contains("yad") {
        Kind::Yad
    } else if name.contains("dmenu") {
        Kind::Dmenu
    } else {
        Kind::Stdin
    }
}

/// A path as given (absolute or relative) or a name to look up on `PATH`.
fn resolve(command: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(command);
    if is_executable(&direct) {
        return Some(direct);
    }
    on_path(command)
}

fn on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn run_menu(menu: &Menu, socket: &Path) -> Result<()> {
    let state = state(socket);
    let items = items_for(&state);
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
    let output = visible_menu_error(show_list(menu, &prompt(&state), &labels))?;
    let Some(label) = checked_choice(&output, menu.kind, "action menu")? else {
        return Ok(()); // cancelled
    };
    let Some(item) = items.into_iter().find(|item| item.label == label.trim()) else {
        let message = format!("action menu returned an unknown choice: {label:?}");
        notify_failure(&message);
        anyhow::bail!(message);
    };
    spawn_action(socket, &item.argv)
}

fn visible_menu_error<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        notify_failure(&format!("menu could not open: {error:#}"));
        error
    })
}

fn checked_choice(output: &Output, kind: Kind, context: &str) -> Result<Option<String>> {
    if !output.status.success() {
        // Rofi, dmenu and the dialog tools use exit 1 for Escape/cancel.
        if output.status.code() == Some(1) && output.stdout.is_empty() {
            return Ok(None);
        }
        let message = format!("{context} failed ({})", output.status);
        notify_failure(&message);
        anyhow::bail!(message);
    }
    let choice = chosen(output, kind);
    Ok((!choice.is_empty()).then_some(choice))
}

/// rofi and dmenu read the list from stdin and print the chosen line.
fn run_stdin_menu(path: &Path, args: &[&str], labels: &[&str]) -> Result<Output> {
    let mut child = Command::new(path)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {}", path.display()))?;
    if let Some(stdin) = child.stdin.as_mut() {
        let mut list = labels.join("\n");
        list.push('\n');
        stdin
            .write_all(list.as_bytes())
            .context("cannot feed the menu")?;
    }
    child.wait_with_output().context("cannot read the menu")
}

fn run_list_menu(path: &Path, args: &[&str], labels: &[&str]) -> Result<Output> {
    Command::new(path)
        .args(args)
        .arg("--column=Action")
        .args(labels)
        .output()
        .with_context(|| format!("cannot run {}", path.display()))
}

fn chosen(output: &Output, kind: Kind) -> String {
    let raw = String::from_utf8_lossy(&output.stdout);
    let line = raw.lines().next().unwrap_or("").trim();
    // yad answers `label|`, echoing the single column it was given.
    match kind {
        Kind::Yad => line.trim_end_matches(|c: char| c == '|' || c.is_whitespace()),
        _ => line,
    }
    .to_string()
}

fn spawn_action(socket: &Path, argv: &[String]) -> Result<()> {
    let exe = env::current_exe().context("cannot locate the pergola executable")?;
    let output = Command::new(exe)
        .arg("--socket")
        .arg(socket)
        .args(argv)
        .output()
        .with_context(|| format!("cannot run `pergola {}`", argv.join(" ")))?;
    // The action's own reply still belongs to whoever ran the palette.
    let _ = std::io::stdout().write_all(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        eprintln!(
            "pergola: palette: `pergola {}` exited with {}: {detail}",
            argv.join(" "),
            output.status
        );
        // The palette's stderr is /dev/null when the daemon launched it, so a
        // failed action would be invisible without a toast. Best effort: a
        // missing notifier never turns a rejected action into a crash.
        let message = if detail.is_empty() {
            format!("pergola {} failed", argv.join(" "))
        } else {
            detail.to_string()
        };
        notify_failure(&message);
        anyhow::bail!("`pergola {}` failed ({})", argv.join(" "), output.status);
    }
    Ok(())
}

fn notify_failure(message: &str) {
    if crate::config::Config::load().notify {
        crate::notify::send(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn first_is(item: &Item, verb: &str) -> bool {
        item.argv.first().map(String::as_str) == Some(verb)
    }

    #[test]
    fn no_workspace_is_offered_until_one_exists() {
        // A desktop that never started a tag must not see a workspace menu.
        assert!(items().iter().all(|i| !first_is(i, "tag-view")));
        assert!(items().iter().all(|i| !first_is(i, "window-tag")));
    }

    #[test]
    fn workspace_rows_name_the_tags_the_document_holds() {
        let state = State {
            title: "xterm".into(),
            tags: vec!["work".into(), "mail".into()],
            active_tag: Some("work".into()),
            ..State::default()
        };
        let rows = items_for(&state);
        // The tag already on screen has nowhere to go, so it is not offered.
        assert!(rows.iter().any(|i| i.label == "Switch to workspace mail"));
        assert!(rows.iter().all(|i| i.label != "Switch to workspace work"));
        // Moving the focused window is offered for every tag, including its own
        // (a no-op there, but the menu cannot know where the window came from).
        for tag in ["work", "mail"] {
            let label = format!("Move this window to workspace {tag}");
            let row = rows.iter().find(|i| i.label == label).expect("no move row");
            assert_eq!(row.argv, vec!["window-tag".to_string(), tag.to_string()]);
        }
        // No focused window, no move rows.
        let bare = items_for(&State {
            tags: vec!["work".into()],
            ..State::default()
        });
        assert!(bare.iter().all(|i| !first_is(i, "window-tag")));
    }

    #[test]
    fn the_tree_yields_the_tags_and_the_active_one() {
        let tree = r#"{
            "doc": {
                "root": {"kind":"panel","id":"p-1","views":["s-1"]},
                "tags": [{"name":"work","views":["s-1"]},{"name":"mail"}],
                "active_tag": "work"
            },
            "pergola": {"seats":[{"seat":"s-1","panel":"p-1","window":1,"title":"t"}]}
        }"#;
        let state = state_from_tree(tree, 1, Some("p-1"));
        assert_eq!(state.tags, vec!["work".to_string(), "mail".to_string()]);
        assert_eq!(state.active_tag.as_deref(), Some("work"));
        // A document without tags loads as no tags at all.
        let plain = state_from_tree(
            r#"{"doc":{"root":{"kind":"panel","id":"p-1"}},"pergola":{"seats":[]}}"#,
            1,
            Some("p-1"),
        );
        assert!(plain.tags.is_empty());
        assert_eq!(plain.active_tag, None);
    }

    #[test]
    fn labels_are_unique() {
        let all = items();
        let mut seen = HashSet::new();
        for item in &all {
            assert!(seen.insert(item.label.clone()), "duplicate label {}", item.label);
        }
        assert_eq!(all.len(), 16);
        // The floating variant swaps one row for another and adds nothing else.
        let floating = items_for(&State {
            title: String::new(),
            floating: true,
            ..State::default()
        });
        assert_eq!(floating.len(), all.len());
        let mut seen = HashSet::new();
        for item in &floating {
            assert!(seen.insert(item.label.clone()), "duplicate label {}", item.label);
        }
    }

    #[test]
    fn every_action_is_a_known_verb() {
        let verbs = [
            "dock",
            "float",
            "tab",
            "ratio",
            "arrange",
            "float-all",
            "zoom-out",
            "frame-window",
            "everything",
            "split-terminal",
            "overlay",
            "undo",
        ];
        for item in items() {
            let first = item.argv.first().expect("empty argv");
            assert!(
                verbs.contains(&first.as_str()),
                "{}: unknown verb {first}",
                item.label
            );
        }
    }

    #[test]
    fn a_floating_window_is_offered_a_tab_not_a_float() {
        let docked = items_for(&State::default());
        let floating = items_for(&State {
            title: "xterm".into(),
            floating: true,
            ..State::default()
        });
        assert!(docked.iter().any(|i| i.label == "Float this window"));
        assert!(floating.iter().all(|i| i.label != "Float this window"));
        assert!(floating.iter().any(|i| i.label == "Fold this window into a tab"));
        // The tile edges are always there: docking a floating window is exactly
        // what they are for.
        assert_eq!(
            docked.iter().filter(|i| first_is(i, "dock")).count(),
            floating.iter().filter(|i| first_is(i, "dock")).count()
        );
    }

    #[test]
    fn the_status_line_yields_the_active_window_and_panel() {
        let status = "pergola 0.5.0-dev · 1600x874 · 2 panels, 0 floating · depth 1 · framed desktop · gap 6 · overlay off · active 0x60000c (p-60000c)";
        assert_eq!(
            parse_active(status),
            Some((0x60000c, Some("p-60000c".to_string())))
        );
        assert_eq!(parse_active("… · active 0x1e (unmanaged)"), Some((0x1e, None)));
        assert_eq!(parse_active("… · active none"), None);
        assert_eq!(parse_active("no active field"), None);
    }

    #[test]
    fn a_panel_in_the_floating_list_is_floating_and_its_title_is_known() {
        let tree = r#"{
            "doc": {
                "root": {"kind":"split","id":"root","axis":"x","children":[
                    {"kind":"panel","id":"p-1","views":["s-1"]},
                    {"kind":"desktop","id":"desktop"}
                ],"weights":[0.5,0.5]},
                "floating": [{"panel": {"id": "p-2", "views": ["s-2"]}, "rect": {}}]
            },
            "pergola": {"seats": [
                {"seat":"s-1","panel":"p-1","window":1,"title":"tiled"},
                {"seat":"s-2","panel":"p-2","window":2,"title":"floating one"}
            ]}
        }"#;
        let floating = state_from_tree(tree, 2, Some("p-2"));
        assert!(floating.floating);
        assert_eq!(floating.title, "floating one");
        let tiled = state_from_tree(tree, 1, Some("p-1"));
        assert!(!tiled.floating);
        assert_eq!(tiled.title, "tiled");
        // No panel (unmanaged) is not floating; the title still comes through.
        assert!(!state_from_tree(tree, 1, None).floating);
    }

    #[test]
    fn menu_programs_map_to_their_own_protocol() {
        assert_eq!(kind_of(Path::new("rofi")), Kind::Stdin);
        assert_eq!(kind_of(Path::new("/usr/bin/dmenu")), Kind::Dmenu);
        assert_eq!(kind_of(Path::new("zenity")), Kind::Zenity);
        assert_eq!(kind_of(Path::new("/usr/bin/yad")), Kind::Yad);
    }

    #[test]
    fn a_broken_menu_path_is_not_a_menu() {
        assert_eq!(resolve("pergola-no-such-menu"), None);
        assert_eq!(resolve("/nonexistent/menu"), None);
    }

    #[test]
    fn a_picked_label_finds_its_action() {
        // `run_menu` maps a menu line back to its argv the same way, with the
        // trimmed label and the same state the list was built from.
        let state = State::default();
        for item in items_for(&state) {
            let picked = format!("  {}  ", item.label);
            let found = items_for(&state)
                .into_iter()
                .find(|i| i.label == picked.trim())
                .map(|i| i.argv);
            assert_eq!(found, Some(item.argv));
        }
        assert!(items_for(&state)
            .into_iter()
            .all(|i| i.label != "nope"));
    }
}
