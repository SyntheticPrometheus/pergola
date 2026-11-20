//! Pergola — a fractal window manager layer for X11 desktops.
//!
//! `pergola daemon` runs the layout engine; every other subcommand is a thin
//! client that forwards to it over a Unix socket.

mod config;
mod daemon;
mod ipc;
mod notify;
mod palette;

use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use pergola_core::{fractal_tree, LayoutEngine, Rect};

fn main() -> ExitCode {
    // A CLI whose output is piped into something that stops early (`pergola
    // config | head`) must die quietly like every other Unix tool. Rust ignores
    // SIGPIPE by default, so the next `println!` panics on a broken pipe and
    // prints a panic message to a terminal that may no longer be looking.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let mut args = std::env::args().skip(1);
    let mut socket = config::socket_path();
    let mut rest = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--socket" {
            let Some(path) = args.next() else {
                eprintln!("pergola: --socket needs a path");
                return ExitCode::FAILURE;
            };
            socket = PathBuf::from(path);
        } else {
            rest.push(arg);
        }
    }

    let Some(command) = rest.first() else {
        print_help();
        return ExitCode::SUCCESS;
    };

    let result: Result<()> = match command.as_str() {
        "daemon" => run_daemon(&socket),
        "doctor" => doctor(),
        "palette" => {
            if rest.iter().any(|a| a == "--list") {
                // What the menu would offer for the focused window, on stdout:
                // a script (and the e2e) can check the state-aware rows without
                // a menu program, the same way `settings --list` works.
                for (label, argv) in palette::list(&socket) {
                    println!("{label}\t{argv}");
                }
                Ok(())
            } else {
                let opened = palette::open(&socket);
                if let Err(error) = &opened {
                    // The application-menu launcher has no terminal to show
                    // stderr. Hotkey failures are notified by the daemon;
                    // respect the same user setting for this CLI path.
                    if config::Config::load().notify {
                        notify::send(&format!("{error:#}"));
                    }
                }
                opened.map(|()| println!("palette starting"))
            }
        }
        "palette-run" => palette::run(&socket),
        "subscribe" => subscribe(&socket),
        "settings" => settings(&socket, &rest[1..]),
        "settings-run" => palette::run_settings(&socket),
        "sim" => sim(&rest[1..]),
        "config" => config_report(&rest[1..]),
        "keys" => keys(&socket, &rest[1..]),
        "bind" => bind(&socket, &rest[1..]),
        "unbind" => unbind(&socket, &rest[1..]),
        "version" | "--version" | "-V" => {
            // The commit is part of the version so a test run can prove which
            // tree it measured (see build.rs and scripts/health.sh).
            println!("pergola {} ({})", pergola_core::VERSION, env!("PERGOLA_COMMIT"));
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        _ => forward(&socket, &rest),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pergola: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_daemon(socket: &PathBuf) -> Result<()> {
    let mut daemon = daemon::Daemon::new()?;
    daemon.run(socket)
}

/// Stream the daemon's event records to stdout, one JSON object per line, until
/// the daemon goes away. This is the status-bar interface: a panel reads this
/// instead of polling `pergola tree` every second.
fn subscribe(socket: &PathBuf) -> Result<()> {
    let reader = ipc::subscribe(socket)?;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // The daemon answers a refused subscription with a Response instead of
        // a record; records never carry an `ok` field, so this is unambiguous.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if value.get("ok") == Some(&serde_json::Value::Bool(false)) {
                anyhow::bail!(
                    "{}",
                    value
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("the daemon refused the subscription")
                );
            }
        }
        println!("{line}");
    }
    eprintln!("pergola: the daemon closed the event stream");
    Ok(())
}

fn forward(socket: &PathBuf, argv: &[String]) -> Result<()> {
    let response = ipc::call(socket, argv)?;
    if response.ok {
        if !response.message.is_empty() {
            println!("{}", response.message);
        }
        Ok(())
    } else {
        anyhow::bail!("{}", response.message)
    }
}

fn doctor() -> Result<()> {
    for note in pergola_x11::doctor()? {
        println!("{note}");
    }
    println!("pergola {}", pergola_core::VERSION);
    Ok(())
}

fn keys(socket: &PathBuf, args: &[String]) -> Result<()> {
    // The plan the daemon would install: defaults merged with the user's
    // per-command overrides from config.json.
    let plan = config::Config::load().binding_plan();
    if !args.iter().any(|a| a == "--check" || a == "--probe") {
        // Ask the running daemon what it actually bound; fall back to the plan.
        if let Ok(response) = ipc::call(socket, &["keys".to_string()]) {
            if response.ok {
                println!("{}", response.message);
                return Ok(());
            }
        }
    }
    if let Some(pos) = args.iter().position(|a| a == "--probe") {
        let x = pergola_x11::X11::connect()?;
        let mut specs: Vec<pergola_x11::KeyBinding> = Vec::new();
        for raw in &args[pos + 1..] {
            let binding = pergola_x11::parse_binding(raw)
                .ok_or_else(|| anyhow::anyhow!("cannot parse binding '{raw}'"))?;
            specs.push(binding);
        }
        let failed = x.grab_keys(&specs)?;
        for binding in &specs {
            let state = if failed.iter().any(|f| f == &binding.label) {
                "TAKEN"
            } else {
                "free "
            };
            println!("{state} {}", binding.label);
        }
        return Ok(());
    }
    if args.iter().any(|a| a == "--check") {
        // A running daemon already owns its grabs. Probing from a fresh client
        // would collide with the daemon itself and report every live chord as
        // TAKEN, which is a lie about a working setup. Ask the daemon what it
        // actually bound first, and only probe when nothing is running.
        if let Ok(response) = ipc::call(socket, &["keys".to_string()]) {
            if response.ok {
                // The daemon prints `<label padded to 24> <command>`, and a
                // command may contain a space ("dock left"), so match the tail.
                // Print the label the daemon actually bound, not the plan's
                // primary chord: when the desktop owns the primary, the daemon
                // binds the fallback (`Super+Up`), and echoing the plan would
                // claim a chord the user cannot use.
                let bound_label = |command: &str| -> Option<String> {
                    let needle = format!(" {command}");
                    response.message.lines().find_map(|line| {
                        line.trim_end()
                            .strip_suffix(&needle)
                            .map(|label| label.trim_end().to_string())
                    })
                };
                for (command, specs) in &plan {
                    match bound_label(command) {
                        Some(label) => println!("bound {:<20} {}", label, command),
                        // An empty candidate list is an intentional unbind, not
                        // a grab that failed: say so instead of crying NONE.
                        None if specs.is_empty() => println!("off   {:<20} {}", "(unbound)", command),
                        None => println!("NONE  {:<20} {}", command, specs.join(" / ")),
                    }
                }
                println!("\n(the daemon is running; these are its live grabs)");
                return Ok(());
            }
        }
        let x = pergola_x11::X11::connect()?;
        let (bound, failures) = x.install_bindings(&plan)?;
        for (binding, command) in &bound {
            println!("bound {:<20} {}", binding.label, command);
        }
        for failure in &failures {
            println!("NONE  {:<20} {}", failure.command, failure.tried.join(" / "));
        }
        return Ok(());
    }
    for (command, specs) in &plan {
        match specs.first() {
            Some(first) => println!("{:<20} {}", first, command),
            None => println!("{:<20} {}", "(unbound)", command),
        }
    }
    Ok(())
}

/// Commands the daemon dispatches by name without going through
/// `Command::parse`. Kept here so `pergola bind` can reject a typo instead of
/// writing a binding that silently never fires.
const DAEMON_COMMANDS: &[&str] = &[
    "status",
    "tree",
    "list",
    "rects",
    "dock",
    "float",
    "arrange",
    "float-all",
    "gap",
    "ratio",
    "overlay",
    "undo",
    "keys",
    "reload",
    "save",
    "stop",
    "split-terminal",
    "palette",
    "settings",
    "url",
    "neighbour",
    "view",
    "save-view",
    "tab",
    "tab-select",
    "tab-next",
    "tab-prev",
    "tag-view",
    "window-tag",
    "tag-remove",
];

/// Would the daemon understand this as a command? Lenient on purpose: a false
/// rejection is worse than a binding that turns out to be a no-op. Accepts a
/// known daemon name, a command URL, or a command plus its arguments.
fn known_command(command: &str) -> bool {
    let first = command.split_whitespace().next().unwrap_or("");
    DAEMON_COMMANDS.contains(&first)
        || pergola_core::Command::parse(command).is_ok()
        || pergola_core::Command::parse(first).is_ok()
}

fn bind(socket: &PathBuf, args: &[String]) -> Result<()> {
    let Some(command) = args.first() else {
        anyhow::bail!(
            "usage: pergola bind <command> [chord...]\n       \
             pergola bind <command>            show what it is bound to\n       \
             pergola bind <command> --default  restore the default chords\n\
             (quote commands with spaces: pergola bind \"view 3\" Super+3)"
        );
    };
    if !known_command(command) {
        anyhow::bail!(
            "'{command}' is not a pergola command\nknown commands: {}",
            DAEMON_COMMANDS.join(", ")
        );
    }
    if args.iter().any(|a| a == "--default") {
        let path = config::write_bindings(command, None)?;
        println!("restored the default chords for '{command}' in {}", path.display());
        return reload_and_report(socket);
    }
    let chords: Vec<String> = args[1..].to_vec();
    if chords.is_empty() {
        return show_binding(command);
    }
    for chord in &chords {
        if pergola_x11::parse_binding(chord).is_none() {
            anyhow::bail!("cannot parse binding '{chord}' (expected e.g. Super+Up or Ctrl+Alt+3)");
        }
    }
    let path = config::write_bindings(command, Some(&chords))?;
    println!(
        "bound '{command}' to {} in {}",
        chords.join(" / "),
        path.display()
    );
    reload_and_report(socket)
}

fn unbind(socket: &PathBuf, args: &[String]) -> Result<()> {
    let Some(command) = args.first() else {
        anyhow::bail!("usage: pergola unbind <command>");
    };
    if !known_command(command) {
        anyhow::bail!(
            "'{command}' is not a pergola command\nknown commands: {}",
            DAEMON_COMMANDS.join(", ")
        );
    }
    let path = config::write_bindings(command, Some(&[]))?;
    println!("unbound '{command}' in {}", path.display());
    reload_and_report(socket)
}

/// Print one command's effective candidates and where they come from.
fn show_binding(command: &str) -> Result<()> {
    let config = config::Config::load();
    let plan = config.binding_plan();
    match plan.iter().find(|(c, _)| c == command) {
        Some((_, specs)) if specs.is_empty() => println!("{command} is unbound"),
        Some((_, specs)) => println!(
            "{command}: {} ({})",
            specs.join(" / "),
            if config.bindings.contains_key(command) {
                "from config.json"
            } else {
                "default"
            }
        ),
        None => println!(
            "{command}: not in the default plan — bind it with `pergola bind {command} <chord>`"
        ),
    }
    Ok(())
}

/// Ask the daemon to reload and print its report. Conflicts and fallbacks come
/// back in the message, so the user sees them where the command ran instead of
/// only in the journal. A daemon that is not running is not an error: the
/// config is already saved and applies at the next start.
fn reload_and_report(socket: &PathBuf) -> Result<()> {
    match ipc::call(socket, &["reload".to_string()]) {
        Ok(response) if response.ok => {
            if !response.message.is_empty() {
                println!("{}", response.message);
            }
            Ok(())
        }
        Ok(response) => anyhow::bail!("saved, but the running daemon rejected reload: {}", response.message),
        Err(_) if !socket.exists() => {
            println!("pergola: saved; no daemon is running, so the change applies at the next start");
            Ok(())
        }
        Err(e) => Err(e.context("saved, but could not reload the running daemon")),
    }
}

/// Headless check: lay the fixture tree out in a 1600x900 frame and print it.
fn sim(args: &[String]) -> Result<()> {
    let engine = LayoutEngine::default();
    let tree = fractal_tree();
    tree.validate()?;
    let frame = Rect::new(0.0, 0.0, 1600.0, 900.0);
    let rects = engine.rects(&tree, frame);
    if args.iter().any(|a| a == "--json") {
        let out: Vec<serde_json::Value> = rects
            .iter()
            .map(|(id, r)| {
                serde_json::json!({"id": id, "x": r.x, "y": r.y, "w": r.w, "h": r.h})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!(
        "pergola {} · fixture depth {} · {} nodes",
        pergola_core::VERSION,
        tree.nesting_depth(),
        rects.len()
    );
    for (id, r) in rects {
        println!(
            "{:<16} {:>6.1} {:>6.1} {:>7.1} {:>7.1}",
            id, r.x, r.y, r.w, r.h
        );
    }
    Ok(())
}

/// `pergola settings` — the settings palette, or a one-shot change.
///
/// The primitive is `settings <key> <value>`: it validates the whole candidate
/// file with the same checks as `config --check`, writes atomically and reloads
/// the daemon, so the menu and a script take the same path.
fn settings(socket: &PathBuf, args: &[String]) -> Result<()> {
    let report = config::Config::inspect();
    match args.first().map(String::as_str) {
        None => {
            palette::open_settings(socket)?;
            println!("settings menu starting");
            Ok(())
        }
        Some("--list") => {
            for (row, _) in config::settings_rows(&report.config) {
                println!("{row}");
            }
            Ok(())
        }
        Some(key) => {
            let value = args[1..].join(" ");
            if value.trim().is_empty() {
                let problem = format!("settings {key} needs a value");
                notify::send(&problem);
                anyhow::bail!(problem);
            }
            // A rejection is the moment the person needs to know: this process
            // was started from a menu or a chord, where stderr goes to the
            // journal. The notification is best effort; the exit code and the
            // message on stderr are unchanged.
            if let Err(e) = config::set_value_at(&config::config_path(), key, &value) {
                notify::send(&format!("{key}: {e:#}"));
                return Err(e);
            }
            let changed = format!("{} = {}", key, value.trim());
            match ipc::call(socket, &["reload".to_string()]) {
                Ok(response) if response.ok => {
                    println!("{changed} · {}", response.message.trim());
                }
                Ok(response) => {
                    let problem = format!("saved {changed}, but the running daemon rejected reload: {}", response.message);
                    notify::send(&problem);
                    anyhow::bail!(problem);
                }
                Err(_) if !socket.exists() => println!("{changed} · saved (no daemon running, applies at next start)"),
                Err(e) => {
                    let problem = format!("saved {changed}, but could not reload the running daemon: {e:#}");
                    notify::send(&problem);
                    anyhow::bail!(problem);
                }
            }
            Ok(())
        }
    }
}

/// `pergola config` — the resolved values and where each one came from, or with
/// `--check`, the problems a bad file has. Reads the file itself, so it works
/// with no daemon running, which is when a typo is most confusing.
fn config_report(args: &[String]) -> Result<()> {
    let report = config::Config::inspect();
    let origin = if report.found {
        format!("{} (found)", report.path.display())
    } else {
        format!("{} (not found — every key is its default)", report.path.display())
    };
    if args.iter().any(|a| a == "--check") {
        println!("{origin}");
        if report.issues.is_empty() {
            println!("  ok    no problems");
            return Ok(());
        }
        for issue in &report.issues {
            println!("  FAIL  {issue}");
        }
        anyhow::bail!(
            "{} problem(s) in {}",
            report.issues.len(),
            report.path.display()
        );
    }
    println!("{origin}");
    for key in config::KNOWN_KEYS {
        println!(
            "  {:<14} {:<26} ({})",
            key,
            config_value(&report.config, key),
            report.source_of(key)
        );
    }
    Ok(())
}

fn config_value(config: &config::Config, key: &str) -> String {
    match key {
        "gap" => format!("{}", config.gap),
        "minimum_share" => format!("{}", config.minimum_share),
        "auto_dock" => match config.auto_dock {
            config::AutoDock::Off => "off".to_string(),
            config::AutoDock::New => "new".to_string(),
            config::AutoDock::All => "all".to_string(),
        },
        "overlay" => config.overlay.to_string(),
        "dim" => format!("{}", config.dim),
        "desktop_share" => format!("{}", config.desktop_share),
        "terminal" => config.terminal.clone(),
        "hotkeys" => config.hotkeys.to_string(),
        "edge_dock" => config.edge_dock.to_string(),
        "bindings" => {
            if config.bindings.is_empty() {
                "{}".to_string()
            } else {
                format!("{} command(s)", config.bindings.len())
            }
        }
        other => format!("<{other}>"),
    }
}

fn print_help() {
    println!(
        r#"pergola {version} — a fractal window manager layer for X11 desktops

USAGE
  pergola daemon                 run the layout daemon (one per display)
  pergola <command> [args]       talk to the running daemon

CAMERA
  everything                     show the whole tree (zoom all the way out)
  zoom-in | zoom-out             step one level in / out
  back | forward                 browser-style history
  frame-window                   frame the focused window (again to go back)
  frame-desktop                  frame the desktop layer
  toggle-desktop                 hide or show floating windows
  neighbour <left|right|up|down> move to the neighbouring space
  view <1-9> | save-view <1-9>   saved views

LAYOUT
  dock <left|right|up|down> [--window ID] [--target ID]
  float [--window ID]
  arrange                        tile every window into a balanced fractal tree
  float-all                      return every window to the desktop layer
  split-terminal                 split the focused window with a new terminal
  gap <pixels>                   change the gap between tiles
  ratio <0.0-1.0> | +d | -d      set the focused panel's share of its split, or nudge it
  undo                           restore the layout before the last change

KEYS
  keys [--check] [--probe <chord>...]  what each command is bound to
  bind <command> [chord...]      bind a command to chords (none: show it)
  bind <command> --default       restore the default chords
  unbind <command>               remove a command's chords

TABS
  tab [--window <id>] [--target <panel>]  fold a window into a panel as a tab
  tab-select <seat|window>       show one view of a tab group
  tab-next | tab-prev            cycle the shown view of the focused group

WORKSPACES
  tag-view [name|all]            switch to a named workspace, `all` to show
                                 everything (no name: list them)
  window-tag <name> [--window <id>]  move a window there without switching
  tag-remove <name>              delete a workspace (its windows stay open)

CONFIG
  config                         the resolved config, each key with its source
  config --check                 reject unknown keys, bad values and bad chords
  settings                       menu of every setting and chord (rofi/zenity/dmenu)
  settings --list                the same rows on stdout
  settings <key> <value>         set one key, validated, then reload

EVENTS
  subscribe                      stream layout, focus, dock, undock and
                                 window-closed records as JSON lines, so a
                                 panel never has to poll `pergola tree`

INSPECTION
  status | tree | list | keys | doctor | sim [--json]
  palette [--list]               menu of tile actions for the focused window (rofi/zenity/dmenu);
                                 --list prints the state-aware rows on stdout
  overlay [on|off|toggle]
  reload | save | stop

FILES
  config   {config}
  layout   {layout}
  socket   {socket}

The daemon never takes SubstructureRedirect, so it runs inside xfwm4, mutter,
kwin_x11, i3, openbox and any other EWMH-compliant X11 desktop."#,
        version = pergola_core::VERSION,
        config = config::config_path().display(),
        layout = config::layout_path().display(),
        socket = config::socket_path().display(),
    );
}
