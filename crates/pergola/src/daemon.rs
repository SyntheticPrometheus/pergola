//! The daemon: owns the tree, reconciles it with the live X11 client list, and
//! applies geometry through the running desktop's window manager.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use pergola_core::engine::replace_node;
use pergola_core::{
    Axis, Camera, CameraAction, Command, Direction, FloatingWindow, GapHandle, History, IRect,
    LayoutDocument, LayoutEngine, LayoutFile, LayoutNode, Rect, SeatRecord,
};
use pergola_x11::{
    command_for_key, describe, event_mods, snap_to_hints, KeyBinding, WindowInfo, X11,
};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ClientMessageEvent, ConfigureNotifyEvent};
use x11rb::protocol::Event;

use crate::config::{self, Config};
use crate::ipc::{Request, Response};

/// Wait on the X connection and the IPC reader together. X11 may already have
/// buffered an event while reading a synchronous reply, so check it first.
fn wait_for_input(x: &X11, wake: &mut UnixStream, timeout: Duration) -> Result<Option<Event>> {
    if let Some(event) = x.poll_event()? {
        return Ok(Some(event));
    }
    let mut fds = [
        libc::pollfd {
            fd: x.conn.stream().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // Both descriptors outlive the call and poll only writes to this array.
    // Round up: truncating a sub-millisecond remainder would busy-poll
    // until the reconciliation or focus deadline.
    let timeout_ms = timeout.as_nanos().div_ceil(1_000_000) as i32;
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout_ms) };
    if ready < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(err.into());
    }
    if fds[1].revents != 0 {
        let mut signals = [0; 256];
        if wake.read(&mut signals)? == 0 {
            anyhow::bail!("IPC listener stopped");
        }
    }
    x.poll_event()
}

static NEXT_LAYOUT_WRITE: AtomicU64 = AtomicU64::new(0);

/// Never truncate the only saved layout. An interrupted write leaves the old
/// document intact; the replacement is renamed into place only after syncing.
fn write_layout_atomically(path: &Path, json: &str) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("layout path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let name = path.file_name().ok_or_else(|| anyhow!("layout path has no filename"))?;
    let prior_permissions = std::fs::metadata(path).ok().map(|m| m.permissions());
    let mut staged = None;
    for _ in 0..32 {
        let serial = NEXT_LAYOUT_WRITE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".{}.{}.{}.tmp",
            name.to_string_lossy(),
            std::process::id(),
            serial
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => {
                staged = Some((temp, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", temp.display())),
        }
    }
    let (temp, mut file) = staged.ok_or_else(|| anyhow!("could not reserve a layout staging file"))?;
    let result = (|| -> Result<()> {
        if let Some(permissions) = prior_permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
            .with_context(|| format!("replacing {}", path.display()))?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// A pointer drag in progress on a split boundary.
struct GapDrag {
    first: String,
    second: String,
    axis: Axis,
    /// Split length along `axis` in screen pixels: divide the pointer delta by
    /// it to get a weight delta.
    extent: f64,
    /// Pointer position at ButtonPress, in root coordinates.
    start: (i32, i32),
    /// Tree when the drag began. Every motion re-derives from this snapshot, so
    /// a drag can never accumulate rounding drift.
    snapshot: LayoutNode,
}

/// What a pointer hint is showing. Enter/Leave arrive for every band window,
/// so the identity of the hovered one is kept: without it a Leave for the band
/// the pointer just left would erase the hint for the band it just entered.
#[derive(Clone, Copy, PartialEq)]
enum Hint {
    None,
    Hover(u32),
    Drop,
}

/// A status-bar client of `pergola subscribe`. The socket is non-blocking so a
/// bar that stops reading can never stall the event loop (bspwm's synchronous
/// `fflush` is the failure this avoids): events queue up to `SUB_QUEUE_MAX`
/// lines. An idle bar is kept — an empty queue means nothing is waiting for it
/// — but a bar with undelivered records that has not drained for `SUB_TIMEOUT`
/// is dropped, so a dead one cannot pin memory either.
struct Subscriber {
    stream: UnixStream,
    queue: std::collections::VecDeque<String>,
    /// Bytes of the front record already written. A non-blocking `write` can
    /// accept a prefix and then return `WouldBlock`; without this the retry
    /// would send the record from the start and the client would see a
    /// duplicated, unparseable line.
    partial: usize,
    /// When the queue was last fully drained; the timeout is measured from it.
    drained: Instant,
}

/// Where a managed window currently lives, for the event stream's diff.
#[derive(Clone, PartialEq)]
enum Placement {
    Docked(String),
    Floating(String),
}

/// A committed state, cheap to compare. Only built when a subscriber exists.
#[derive(Clone, PartialEq)]
struct EventSig {
    /// The tree as JSON, so a `layout` record can carry it without a second
    /// serialisation.
    root: String,
    /// Window -> (placement, title). A window that leaves this map closed.
    placements: std::collections::BTreeMap<u32, (Placement, String)>,
    active: u32,
    /// The active tag, so a bar can label the workspace without parsing the
    /// tree. `None` is "no tag": every view is shown.
    tag: Option<String>,
}

/// One undoable layout state: the document (tree, floating rects, tabs), the
/// engine (so a gap change is undone too) and where the camera was looking.
#[derive(Clone, PartialEq)]
struct Snapshot {
    doc: LayoutDocument,
    engine: LayoutEngine,
    camera: pergola_core::CameraState,
}

/// A window that may have been dragged by the user, watched for the moment it
/// stops moving (edge docking: drop a window on a screen edge to tile it).
struct MoveWatch {
    /// Configure events that actually changed the geometry since the window
    /// left the position we last applied.
    moves: u32,
    at: Instant,
    rect: Rect,
    /// Where the window was when it was first seen moving: a drag is judged by
    /// the distance travelled, not only by how many events it produced.
    start: Rect,
}

/// A configure event that arrived while our own apply was still settling.
/// The newest rect per window is kept and re-tested once the guard expires:
/// openbox reports a whole drag as one `ConfigureNotify` right after the daemon
/// re-applies the layout, so discarding it loses the drop.
struct DeferredConfigure {
    rect: Rect,
    /// When the live rect was first seen to match `rect` after the guard. A
    /// stale echo of an intermediate apply step moves again, so it must not be
    /// judged until it has stood still.
    stable: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DeferredAction {
    /// Our own apply landed (or the window is gone): the event was an echo.
    Discard,
    /// The window is still moving, or the event was stale: re-read it later.
    Restart(Rect),
    /// The live rect matches the event; the stability clock is running.
    Wait,
    /// The live rect has matched the event long enough to be a real drop.
    Ready(Rect),
}

/// Decide what a deferred configure means now that the apply guard has expired.
fn deferred_action(
    applied: Option<Rect>,
    remembered: Rect,
    live: Rect,
    stable: Option<Instant>,
    now: Instant,
) -> DeferredAction {
    if applied.is_some_and(|a| same_rect_within(a, live, DECORATION_SLOP_PX)) {
        return DeferredAction::Discard;
    }
    if !same_rect(live, remembered) {
        return DeferredAction::Restart(live);
    }
    match stable {
        Some(since) if now.duration_since(since) >= DEFERRED_STABLE => {
            DeferredAction::Ready(live)
        }
        _ => DeferredAction::Wait,
    }
}

pub struct Daemon {
    x: X11,
    engine: LayoutEngine,
    file: LayoutFile,
    camera: pergola_core::CameraState,
    config: Config,
    overlay_on: bool,
    bindings: Vec<(KeyBinding, String)>,
    keymap: Vec<(u8, u32)>,
    seats: Vec<SeatRecord>,
    /// Seated clients with a confirmed property/structure event subscription.
    watched: HashSet<u32>,
    /// Last focused tiled panel. A newly focused floating window must not
    /// replace the docking target it will be placed beside.
    last_focus: Option<String>,
    /// Desktop-space rectangle from the previous apply, used to notice that the
    /// desktop leaf moved (and floating windows must be remapped).
    last_desk: Option<Rect>,
    /// Force floating windows to their stored rect on the next apply.
    place_floating: bool,
    /// Whether the previous apply ran under a camera transform. Live rects seen
    /// while zoomed are screen-space, not world-space, so they must not be
    /// adopted back as the window's stored position.
    last_zoomed: bool,
    running: bool,
    socket_path: PathBuf,
    layout_path: PathBuf,
    /// Split boundaries exposed to the pointer, as (grab window, handle).
    gap_bands: Vec<(u32, GapHandle)>,
    /// The gap drag in progress, if any.
    drag: Option<GapDrag>,
    /// The frame rectangle we last asked the WM to produce, per window. A
    /// configure that differs from this is the user moving the window, not us.
    applied: HashMap<u32, IRect>,
    /// Windows whose configure stream is being watched for a drop on an edge.
    moves: HashMap<u32, MoveWatch>,
    /// Configures seen inside the own-apply guard, re-tested once it expires.
    deferred: HashMap<u32, DeferredConfigure>,
    /// When our own apply last ran; edge detection ignores its event wake.
    last_apply: Instant,
    /// The work area of the last apply, the frame the `applied` rects were
    /// computed in. A configure for a window we placed outside it is the WM
    /// clamping a parked window back into view, not a gesture.
    last_screen: Option<IRect>,
    /// The last `KeyRelease` seen: keycode and server timestamp. The server
    /// auto-repeats a held key by sending a `KeyRelease` and a `KeyPress` with
    /// the *same* timestamp, so that pair is a repeat and the press is dropped.
    last_release: Option<(u8, u32)>,
    /// False until the daemon's first reconcile finishes. `auto_dock: new`
    /// docks windows that appear after this, `all` also adopts the ones that
    /// were already on screen at startup.
    started: bool,
    /// Layout states to come back to, newest last. Bounded: ten steps.
    undo: History<Snapshot>,
    /// The pointer hint currently on screen (hover line or drop frame).
    hint: Hint,
    /// The edge the drop frame is showing, so a stream of ConfigureNotify
    /// events does not rebuild the same four windows on every pixel.
    drop_edge: Option<Direction>,
    /// `pergola subscribe` clients: panel/status-bar processes that want the
    /// layout pushed instead of polling `pergola tree` (i3 IPC, bspwm report).
    subscribers: Vec<Subscriber>,
    /// Windows we recently asked the desktop to focus. A `_NET_ACTIVE_WINDOW`
    /// request is broadcast to every client selecting root's substructure
    /// events — including us — so each of our own requests comes back as an
    /// event. Handling it as somebody else's would send it again, forever.
    /// A small list, not one slot: two focuses in a row must both be recognised
    /// when their echoes arrive in order.
    own_activations: Vec<(u32, Instant)>,
}

impl Daemon {
    pub fn new() -> Result<Self> {
        let x = X11::connect()?;
        let config = Config::load();
        let layout_path = config::layout_path();
        let file = match std::fs::read_to_string(&layout_path) {
            Ok(raw) => LayoutFile::from_json(&raw).unwrap_or_else(|e| {
                eprintln!("pergola: ignoring malformed layout: {e}");
                LayoutFile::default()
            }),
            Err(_) => LayoutFile::default(),
        };
        let engine = LayoutEngine::new(config.gap, config.minimum_share)
            .with_desktop_share(config.desktop_share);
        let camera = file.camera();
        let seats = file
            .pergola
            .as_ref()
            .map(|p| p.seats.clone())
            .unwrap_or_default();
        // The config is the default; a toggle only lasts for the session, so a
        // persisted `overlay` value must not override a config edit.
        let overlay_on = config.overlay;
        let bindings: Vec<(KeyBinding, String)> = Vec::new();
        let keymap = build_keymap(&x)?;
        let mut daemon = Self {
            x,
            engine,
            file,
            camera,
            config,
            overlay_on,
            bindings,
            keymap,
            seats,
            watched: HashSet::new(),
            last_focus: None,
            last_desk: None,
            place_floating: true,
            last_zoomed: false,
            running: true,
            socket_path: config::socket_path(),
            layout_path,
            gap_bands: Vec::new(),
            drag: None,
            applied: HashMap::new(),
            moves: HashMap::new(),
            deferred: HashMap::new(),
            last_apply: Instant::now(),
            last_screen: None,
            last_release: None,
            started: false,
            undo: History::new(UNDO_DEPTH),
            hint: Hint::None,
            drop_edge: None,
            subscribers: Vec::new(),
            own_activations: Vec::new(),
        };
        daemon.reconcile()?;
        daemon.started = true;
        daemon.normalize_camera();
        daemon.apply()?;
        Ok(daemon)
    }

    // MARK: reconciliation

    /// Match the tree against the live client list: drop panels whose window is
    /// gone, adopt new windows, refresh titles.
    fn reconcile(&mut self) -> Result<bool> {
        let windows = self.x.client_list()?;
        // `_NET_CLIENT_LIST` still contains a minimised window (and windows on
        // the host WM's other desktops) even though it is unmapped. A seat is
        // gone only when its window leaves the client list: dropping it because
        // the window was merely minimised collapses the tile and loses the
        // layout — i3#718, awesome#92.
        let clients: HashSet<u32> = windows.iter().copied().collect();

        let gone: Vec<SeatRecord> = self
            .seats
            .iter()
            .filter(|s| !clients.contains(&s.window))
            .cloned()
            .collect();
        let mut changed = !gone.is_empty();
        for seat in gone {
            self.watched.remove(&seat.window);
            self.applied.remove(&seat.window);
            self.moves.remove(&seat.window);
            self.deferred.remove(&seat.window);
            self.seats.retain(|s| s.seat != seat.seat);
            // A tab group loses one view, not the whole panel: closing a tab
            // must leave its siblings where they are. A panel whose last view
            // goes collapses through the same path.
            self.detach_seat(&seat)?;
            // Only a closed window leaves its workspace. Docking, floating,
            // and tabbing also detach seats while moving them within a layout.
            self.file.doc.forget_view(&seat.seat);
            self.file.doc.floating.retain(|f| f.panel.id != seat.panel);
        }

        for win in windows {
            if self.watched.contains(&win) {
                continue;
            }
            if self.seats.iter().any(|seat| seat.window == win) {
                self.watch_seat(win);
                continue;
            }
            let Ok(Some(info)) = self.x.info(win) else { continue };
            let w = &info;
            if w.hidden {
                continue;
            }
            // A dialog or popup (`WM_TRANSIENT_FOR`) belongs to its parent and
            // the WM places it. Adopting it would create a seat for a window
            // that `apply` then refuses to move, leaving an empty panel in the
            // tree and a tile with nothing in it — awesome#22, i3#787.
            if w.transient {
                continue;
            }
            // A window on another host desktop is not part of this desktop's
            // layout; adopt it when the user brings it here. Adopting it now
            // would put it in the tree and `apply` would move it back.
            if on_another_desktop(self.x.current_desktop(), self.x.desktop_of(w.id))
            {
                continue;
            }
            let seat = format!("s-{:x}", w.id);
            let mut record = SeatRecord {
                seat: seat.clone(),
                panel: format!("p-{:x}", w.id),
                window: w.id,
                class: w.class.clone(),
                title: w.title.clone(),
            };
            // X11 can reuse a closed window's ID while its old tab group still owns the panel.
            let panel = self.free_panel_id(&record);
            record.panel = panel.clone();
            self.seats.push(record);
            changed = true;
            // A new window joins the tag the user is looking at. Without this
            // it would belong to no tag and the active view would prune it
            // away, so the window would open invisible.
            if let Some(tag) = self.file.doc.active_tag.clone() {
                self.file.doc.assign_view(&seat, &tag);
            }
            if self.config.auto_dock.docks(self.started) {
                // The WM may have already focused the new window, so
                // `focused_panel` can name a panel that is not on screen yet
                // (its own). Dock beside a panel that is actually there, or the
                // whole shown tree when nothing docked has focus.
                let shown = self.file.doc.active_tree();
                let target = self
                    .focused_panel()
                    .filter(|p| shown.node(p).is_some())
                    .or_else(|| {
                        self.last_focus
                            .clone()
                            .filter(|p| shown.node(p).is_some())
                    })
                    .unwrap_or_else(|| shown.id().to_string());
                let node = LayoutNode::panel(panel.clone(), vec![seat.clone()]);
                match self
                    .engine
                    .inserting(node, &target, Direction::Right, &self.file.doc.root)
                {
                    Ok(tree) => self.replace_root(tree),
                    Err(_) => self.push_floating(w, panel, seat),
                }
            } else {
                self.push_floating(w, panel, seat);
            }
            self.watch_seat(win);
        }
        Ok(changed)
    }

    fn watch_seat(&mut self, win: u32) {
        let subscribed = self.x.watch_window(win).is_ok();
        // Read after selecting notifications: a change during adoption must
        // either be reflected in this read or be queued on our connection.
        if let Ok(Some(info)) = self.x.info(win) {
            if let Some(seat) = self.seats.iter_mut().find(|s| s.window == win) {
                seat.title = info.title;
                seat.class = info.class;
                if subscribed {
                    self.watched.insert(win);
                }
            }
        }
    }

    fn reconcile_layout(&mut self) -> Result<()> {
        if self.reconcile()? {
            if self.drag.is_some() {
                let _ = self.end_gap_drag();
            }
            self.normalize_camera();
            let _ = self.apply();
            let _ = self.save();
        }
        Ok(())
    }

    fn push_floating(&mut self, w: &WindowInfo, panel: String, seat: String) {
        let rect = w.frame_rect().to_rect();
        self.file.doc.floating.push(FloatingWindow {
            panel: pergola_core::Panel::new(panel, vec![seat]),
            rect,
            size: pergola_core::Size::new(w.rect.w, w.rect.h),
            z: None,
        });
    }

    /// Keep the camera pointing at a node that exists. An empty or stale
    /// `framed` means "the whole tree".
    fn normalize_camera(&mut self) {
        // The screen shows the active tag's view of the tree, so a camera
        // pointing at a node on another tag would frame nothing. Reset it to
        // the shown tree's own id, which means "everything".
        //
        // A framing on the empty desktop leaf is the same mistake in another
        // form: when a window joins that tag the desktop becomes one tile
        // beside it, and "framed desktop" would zoom into the empty tile and
        // push every window off screen. The desktop leaf is never a useful
        // thing to frame while there is a panel to show.
        let shown = self.file.doc.active_tree();
        let has_panel = shown
            .flatten()
            .iter()
            .any(|n| matches!(n, LayoutNode::Panel(_)));
        let framed_desktop = matches!(
            shown.node(&self.camera.framed),
            Some(LayoutNode::Desktop(_))
        );
        if self.camera.framed.is_empty()
            || shown.node(&self.camera.framed).is_none()
            || (framed_desktop && has_panel)
        {
            self.camera.framed = shown.id().to_string();
        }
    }

    /// Replace the tree, keeping "everything" framed on the new root.
    fn replace_root(&mut self, new_root: LayoutNode) {
        let old = self.file.doc.root.id().to_string();
        if self.camera.framed == old {
            self.camera.framed = new_root.id().to_string();
        }
        self.file.doc.root = new_root;
    }

    /// Is this window on a host desktop the WM is not showing? See
    /// [`on_another_desktop`] for why it is left alone there.
    fn foreign_desktop(&self, window: u32) -> bool {
        on_another_desktop(self.x.current_desktop(), self.x.desktop_of(window))
    }

    /// The frame the `applied` rects were computed in (see `last_screen`).
    fn last_screen(&self) -> Option<Rect> {
        self.last_screen.map(|r| r.to_rect())
    }

    fn focused_panel(&self) -> Option<String> {
        let active = self.x.active_window()?;
        let seat = self.seats.iter().find(|s| s.window == active)?;
        Some(seat.panel.clone())
    }

    fn seat_for_window(&self, win: u32) -> Option<&SeatRecord> {
        self.seats.iter().find(|s| s.window == win)
    }

    /// Take one seat out of the tree wherever it lives: out of a tab group
    /// (only that view — its siblings stay), out of a plain panel (which then
    /// leaves the tree like any node), or off the desktop layer. The seat
    /// record stays, so a caller can put the window back somewhere else.
    fn detach_seat(&mut self, seat: &SeatRecord) -> Result<()> {
        self.file
            .doc
            .floating
            .retain(|f| !f.panel.views.iter().any(|v| v == &seat.seat));
        // `removing_view` handles both cases: a group loses one view, and an
        // empty panel collapses through the same rules as `removing`.
        if let Ok(tree) = self.engine.removing_view(&seat.seat, &self.file.doc.root) {
            self.replace_root(tree);
        }
        Ok(())
    }

    /// A panel id the seat can own outright. A seat leaving a tab group needs
    /// a fresh id because the group keeps its own; a seat that owned its panel
    /// can keep the id. The seat record is updated so `focused_panel` and
    /// `list` stay truthful afterwards.
    fn own_panel_id(&mut self, seat: &SeatRecord) -> String {
        let id = self.free_panel_id(seat);
        if let Some(record) = self.seats.iter_mut().find(|s| s.seat == seat.seat) {
            record.panel = id.clone();
        }
        id
    }

    /// The same id `own_panel_id` would pick, without touching the seat record.
    /// A preview must not move a seat it is only drawing a frame for.
    fn free_panel_id(&self, seat: &SeatRecord) -> String {
        let taken = |id: &str| -> bool {
            self.file.doc.root.node(id).is_some()
                || self.file.doc.floating.iter().any(|f| f.panel.id == id)
        };
        let mut id = seat.panel.clone();
        if taken(&id) {
            let base = format!("p-{:x}", seat.window);
            id = base.clone();
            let mut n = 1;
            while taken(&id) {
                id = format!("{base}-{n}");
                n += 1;
            }
        }
        id
    }

    // MARK: applying geometry

    /// The dim layer the overlay should draw, or `None` when the screen cannot
    /// show one honestly: a compositor must own `_NET_WM_CM_S<screen>` to read
    /// `_NET_WM_WINDOW_OPACITY`, and the server needs SHAPE so a full-screen
    /// layer does not swallow clicks meant for the tiles under it. With either
    /// missing the split lines alone are the overlay, exactly as before.
    fn dim_layer(&self, screen: IRect) -> Option<(IRect, u32)> {
        if !self.overlay_on || !self.x.compositor_active() || !self.x.shape_available() {
            return None;
        }
        let opacity = (self.config.dim.clamp(0.0, 0.9) * u32::MAX as f64).round() as u32;
        if opacity == 0 {
            return None;
        }
        Some((screen, opacity))
    }

    /// The layout as it stands, for the undo ring.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            doc: self.file.doc.clone(),
            engine: self.engine,
            camera: self.camera.clone(),
        }
    }

    /// Put the layout back the way it was before the last change. Windows that
    /// have closed since the snapshot are dropped by the reconcile, so undo
    /// cannot resurrect a seat; floating windows are placed back at their
    /// stored rectangles.
    fn undo_layout(&mut self) -> Result<String> {
        let Some(previous) = self.undo.pop() else {
            return Ok("nothing to undo".to_string());
        };
        // The layout we are restoring is a new baseline for mouse movement.
        // A non-drag watch can survive an earlier drop; otherwise Openbox's
        // 1px configure echo after this apply can turn that stale watch into
        // a false drag and immediately undo the restoration.
        self.moves.clear();
        self.deferred.clear();
        self.hide_drop_hint()?;
        let prior_floating: HashMap<String, FloatingWindow> = self
            .file
            .doc
            .floating
            .iter()
            .flat_map(|item| {
                item.panel
                    .views
                    .iter()
                    .map(move |seat| (seat.clone(), item.clone()))
            })
            .collect();
        let prior_tags: HashMap<String, String> = self
            .seats
            .iter()
            .filter_map(|seat| {
                self.file
                    .doc
                    .tag_of(&seat.seat)
                    .map(|tag| (seat.seat.clone(), tag.to_string()))
            })
            .collect();
        self.file.doc = previous.doc;
        self.engine = previous.engine;
        self.camera = previous.camera;
        self.reconcile()?;
        self.restore_unplaced_seats(&prior_floating, &prior_tags)?;
        self.normalize_camera();
        self.place_floating = true;
        self.apply()?;
        self.save()?;
        let left = self.undo.len();
        Ok(if left == 0 {
            "undid the last layout change (nothing older left)".to_string()
        } else {
            format!("undid the last layout change ({left} older to undo)")
        })
    }

    /// Undo restores an older document, but windows opened since that snapshot
    /// are still live and already have seat records. Reconcile therefore does
    /// not adopt them again; keep each one on the floating layer instead of
    /// silently losing it from the layout and its status count.
    fn restore_unplaced_seats(
        &mut self,
        prior_floating: &HashMap<String, FloatingWindow>,
        prior_tags: &HashMap<String, String>,
    ) -> Result<()> {
        let mut placed = HashSet::new();
        for node in self.file.doc.root.flatten() {
            if let LayoutNode::Panel(panel) = node {
                placed.extend(panel.views.iter().cloned());
            }
        }
        for item in &self.file.doc.floating {
            placed.extend(item.panel.views.iter().cloned());
        }
        let unplaced: Vec<SeatRecord> = self
            .seats
            .iter()
            .filter(|seat| !placed.contains(&seat.seat))
            .cloned()
            .collect();
        for seat in unplaced {
            let Some(info) = self.x.info(seat.window)? else {
                continue;
            };
            let panel = self.own_panel_id(&seat);
            if let Some(mut saved) = prior_floating.get(&seat.seat).cloned() {
                saved.panel = pergola_core::Panel::new(panel, vec![seat.seat.clone()]);
                self.file.doc.floating.push(saved);
            } else {
                self.push_floating(&info, panel, seat.seat.clone());
            }
            // A window created after the snapshot can belong to another
            // workspace. Preserve that assignment even if the older snapshot
            // did not contain the workspace yet.
            if let Some(tag) = prior_tags.get(&seat.seat) {
                self.file.doc.assign_view(&seat.seat, tag);
            }
        }
        Ok(())
    }

    fn apply(&mut self) -> Result<()> {
        let screen = self.x.workarea_rect()?;
        // No usable output geometry yet (server still starting, monitor asleep,
        // RandR mid-change): every rect computed from a 0x0 world divides by
        // zero and collapses the whole tree. Leave the last geometry alone and
        // let the next reconcile try again.
        if screen.w <= 0 || screen.h <= 0 {
            return Ok(());
        }
        self.last_screen = Some(screen);
        let world = screen.to_rect();
        // The camera must point at a node that exists in the tree being drawn.
        // A command may move the last window off a tag, leaving the camera
        // framed on the empty desktop leaf; `apply` is the one place every
        // change passes through, so normalize here as well as in the commands.
        self.normalize_camera();
        // Only the active tag's views are on screen. With no tag active this is
        // the whole tree, so a user who never names a workspace sees no change.
        let shown = self.file.doc.active_tree();
        let rects = self.engine.rects(&shown, world);
        let framed = if self.camera.framed != shown.id() {
            rects
                .iter()
                .find(|(id, _)| id == &self.camera.framed)
                .map(|(_, r)| *r)
        } else {
            None
        };
        let tf = |r: Rect| transform(r, framed, world);

        for (id, rect) in &rects {
            let Some(LayoutNode::Panel(panel)) = shown.node(id) else {
                continue;
            };
            let target = park_if_outside(tf(*rect).to_irect(), screen, framed.is_some());
            for view in &panel.views {
                let Some(seat) = self.seats.iter().find(|s| &s.seat == view) else {
                    continue;
                };
                let Ok(Some(state)) = self.x.window_state(seat.window) else {
                    continue;
                };
                // A fullscreen window owns its geometry: the WM has taken it
                // over and moving it would fight the user's F11. A transient
                // window (dialog, popup) belongs to its parent, and tiling it
                // lands it wherever the parent happens to be — awesome#22.
                // Leave both alone until the state clears.
                if state.fullscreen || state.transient {
                    continue;
                }
                // On another host desktop the window belongs to the WM there;
                // moving it from here is what pulled a parked window back.
                if self.foreign_desktop(seat.window) {
                    continue;
                }
                // A maximized window ignores `_NET_MOVERESIZE_WINDOW`: the
                // tile would keep the full-screen geometry and the tree would
                // lie. Drop the state, then move.
                if state.maximized {
                    let _ = self.x.unmaximize(seat.window);
                }
                // Snap the client area to its own resize grid, so a terminal
                // lands flush against the tile instead of being clamped by the
                // WM to an arbitrary shortfall.
                let target = snap_to_hints(target, self.x.size_hints(seat.window), self.x.frame_extents(seat.window));
                let _ = self.x.move_resize(seat.window, target);
                self.applied.insert(seat.window, target);
            }
            if let Some(visible) = panel.visible() {
                if let Some(seat) = self.seats.iter().find(|s| s.seat == visible) {
                    // Raising a window on another host desktop is not a no-op:
                    // xfwm4 answers the raise by mapping it on the *current*
                    // desktop (measured 2026-11-17: `arrange` re-docked a
                    // floating window parked on desktop 3 and it came back).
                    // The WM there owns it, so leave it unraised too.
                    if !self.foreign_desktop(seat.window) {
                        let _ = self.x.raise(seat.window);
                    }
                }
            }
        }

        // Windows on a tag that is not shown leave the screen: park them below
        // the work area. They keep their seat and their place in the full tree,
        // so switching back restores the layout exactly.
        self.park_inactive(&shown, screen)?;

        // The desktop leaf is a space, not the whole screen: floating windows
        // are mapped into whatever rectangle it currently occupies. Their stored
        // rects stay in full-screen ("world") coordinates, so a drag is undone
        // into world space and the layout never fights the user.
        let desk = rects
            .iter()
            .find(|(id, _)| id == "desktop")
            .map(|(_, r)| *r);
        let desk_changed = self
            .last_desk
            .map_or(true, |d| !same_rect(d, desk.unwrap_or(world)));
        let place = self.place_floating || desk_changed || self.last_zoomed;
        let mut floating = std::mem::take(&mut self.file.doc.floating);
        for f in floating.iter_mut() {
            let window = f
                .panel
                .views
                .first()
                .and_then(|view| self.seats.iter().find(|s| &s.seat == view))
                .map(|s| s.window);
            let Some(window) = window else { continue };
            // A floating window on another host desktop stays where the WM put
            // it; its stored rectangle is unchanged, so returning to this
            // desktop restores it exactly like a tag switch does.
            if self.foreign_desktop(window) {
                continue;
            }
            // A floating window belongs to a tag through its view. On another
            // tag it leaves the screen, but its stored rectangle stays as the
            // user left it — parking must not overwrite it.
            if !f
                .panel
                .views
                .first()
                .map(|v| self.file.doc.view_visible(v))
                .unwrap_or(true)
            {
                let target = IRect::new(
                    screen.x,
                    screen.y + screen.h,
                    f.rect.w as i32,
                    f.rect.h as i32,
                );
                if self.applied.get(&window) != Some(&target) {
                    let _ = self.x.move_resize(window, target);
                    self.applied.insert(window, target);
                }
                continue;
            }
            if self.camera.desktop_hidden {
                let target = IRect::new(
                    screen.x,
                    screen.y + screen.h,
                    f.rect.w as i32,
                    f.rect.h as i32,
                );
                let _ = self.x.move_resize(window, target);
                self.applied.insert(window, target);
            } else if framed.is_some() {
                let target = tf(into_screen(f.rect, desk, world)).to_irect();
                let _ = self.x.move_resize(window, target);
                self.applied.insert(window, target);
            } else if place {
                let target = into_screen(f.rect, desk, world).to_irect();
                let _ = self.x.move_resize(window, target);
                self.applied.insert(window, target);
            } else if let Ok(Some(info)) = self.x.info(window) {
                // The desktop space did not move, so the window is where the
                // user put it: learn the new position instead of snapping back.
                f.rect = into_world(info.frame_rect().to_rect(), desk, world);
            }
        }
        self.file.doc.floating = floating;
        self.last_desk = Some(desk.unwrap_or(world));
        self.last_zoomed = framed.is_some();
        self.place_floating = false;

        let dim = self.dim_layer(screen);
        if self.overlay_on {
            let framed_id = if self.camera.framed == shown.id() {
                None
            } else {
                Some(self.camera.framed.as_str())
            };
            // Without a dim layer the lines are all the overlay has, so they
            // are drawn twice as thick: 4 px reads on a bare X server and in a
            // screenshot, where a 2 px line in a 6 px gap all but disappears.
            let bars: Vec<(IRect, u32)> = overlay_bars(
                &shown,
                &rects,
                framed_id,
                self.engine.gap,
                world,
                dim.is_none(),
            )
            .into_iter()
            .map(|(bar, color)| {
                let r = tf(bar.to_rect());
                (
                    IRect::new(
                        r.x.round() as i32,
                        r.y.round() as i32,
                        r.w.round().max(1.0) as i32,
                        r.h.round().max(1.0) as i32,
                    ),
                    color,
                )
            })
            .collect();
            self.x.overlay_show(&bars, dim)?;
        } else {
            self.x.overlay_hide()?;
        }
        // The pointer-facing boundaries follow every layout change — except
        // mid-drag, where replacing the windows under the pointer would abort
        // the drag.
        if self.drag.is_none() {
            self.hide_hint()?;
            self.refresh_gap_bands()?;
        }
        self.last_apply = Instant::now();
        self.x.conn.flush()?;
        Ok(())
    }

    /// Rebuild the invisible grab areas for every draggable split boundary, in
    /// screen coordinates (so they follow the camera).
    fn refresh_gap_bands(&mut self) -> Result<()> {
        let screen = self.x.workarea_rect()?;
        let world = screen.to_rect();
        let shown = self.file.doc.active_tree();
        let rects = self.engine.rects(&shown, world);
        let framed = if self.camera.framed != shown.id() {
            rects
                .iter()
                .find(|(id, _)| id == &self.camera.framed)
                .map(|(_, r)| *r)
        } else {
            None
        };
        let tf = |r: Rect| transform(r, framed, world);
        let mut handles = self.engine.gap_handles(&shown, world);
        let mut bands = Vec::new();
        for h in &mut handles {
            let shown = tf(h.band);
            // The camera transform is non-uniform, so the pixel→weight
            // denominator must use the axis the boundary moves on.
            let scale = axis_scale(h.axis, h.band, shown);
            h.band = shown;
            h.extent *= scale;
            bands.push((
                IRect::new(
                    shown.x.round() as i32,
                    shown.y.round() as i32,
                    shown.w.round().max(1.0) as i32,
                    shown.h.round().max(1.0) as i32,
                ),
                h.axis,
            ));
        }
        let ids = self.x.grab_bands_show(&bands)?;
        self.gap_bands = ids.into_iter().zip(handles).collect();
        Ok(())
    }

    /// Move every window whose view is not on the active tag below the work
    /// area, so switching tags really clears the screen. The seat, its panel and
    /// its place in the full tree stay put, so switching back restores the
    /// layout exactly. A window the WM already reports as hidden (minimised),
    /// fullscreen or transient is left alone: its geometry is not ours.
    fn park_inactive(&mut self, shown: &LayoutNode, screen: IRect) -> Result<()> {
        let mut views = std::collections::HashSet::new();
        for node in shown.flatten() {
            if let LayoutNode::Panel(p) = node {
                views.extend(p.views.iter().cloned());
            }
        }
        for seat in &self.seats {
            if views.contains(&seat.seat) {
                continue;
            }
            // A floating window is parked by the floating loop, which knows to
            // keep its stored rectangle.
            if self
                .file
                .doc
                .floating
                .iter()
                .any(|f| f.panel.views.iter().any(|v| v == &seat.seat))
            {
                continue;
            }
            let Ok(Some(info)) = self.x.info(seat.window) else {
                continue;
            };
            if info.hidden || info.fullscreen || info.transient {
                continue;
            }
            if self.foreign_desktop(seat.window) {
                continue;
            }
            let frame = info.frame_rect();
            let target = IRect::new(screen.x, screen.y + screen.h, frame.w, frame.h);
            // Re-parking an already parked window is a wasted move and a
            // configure event the drag watcher would have to ignore.
            if self.applied.get(&seat.window) == Some(&target) {
                continue;
            }
            let _ = self.x.move_resize(seat.window, target);
            self.applied.insert(seat.window, target);
        }
        Ok(())
    }

    /// Copy the live seat records into the document's metadata. `tree` reads
    /// the document, not the daemon's own `seats`, so a title or class change
    /// handled by `PropertyNotify` must reach the metadata or the read-only
    /// `tree` view keeps showing the name the window had when it was adopted.
    fn sync_meta(&mut self) {
        let meta = pergola_core::PergolaMeta {
            seats: self.seats.clone(),
            gap: Some(self.engine.gap),
            auto_dock: Some(self.config.auto_dock != crate::config::AutoDock::Off),
            overlay: Some(self.overlay_on),
        };
        self.file.pergola = Some(meta);
    }

    fn save(&mut self) -> Result<()> {
        self.file.set_camera(&self.camera);
        self.sync_meta();
        write_layout_atomically(&self.layout_path, &self.file.to_json())?;
        Ok(())
    }

    // MARK: commands

    pub fn handle(&mut self, argv: &[String]) -> Result<String> {
        // A command that arrives mid-drag (a hotkey, an IPC client) ends the
        // pointer gesture: its tree snapshot would otherwise be reinstated by
        // the next motion event.
        if self.drag.is_some() {
            let _ = self.end_gap_drag();
        }
        // Snapshot before the command so a dock, float, arrange or gap change
        // can be undone. `undo` is exempt: recording the state it replaces
        // would let undo oscillate between two layouts.
        let track = !matches!(
            argv.first().map(String::as_str),
            Some("undo" | "status" | "tree" | "list" | "rects" | "keys")
        );
        let before = track.then(|| self.snapshot());
        let result = self.dispatch(argv);
        if let Some(before) = before {
            let changed = self.file.doc != before.doc
                || self.engine != before.engine
                || self.camera != before.camera;
            if result.is_ok() && changed {
                self.undo.push(before);
            }
        }
        if let Some(panel) = self.focused_panel() {
            if self.file.doc.root.node(&panel).is_some() {
                self.last_focus = Some(panel);
            }
        }
        result
    }

    fn dispatch(&mut self, argv: &[String]) -> Result<String> {
        let Some(name) = argv.first().map(String::as_str) else {
            return Err(anyhow!("empty command"));
        };
        let rest = &argv[1..];
        match name {
            "status" => self.status(),
            "tree" => {
                self.sync_meta();
                Ok(self.file.to_json())
            }
            "list" => self.list(),
            "rects" => self.rects(rest),
            "dock" => self.dock(rest),
            "float" => self.float(rest),
            "arrange" => self.arrange(),
            "float-all" => self.float_all(),
            "gap" => self.set_gap(rest),
            "ratio" => self.set_ratio(rest),
            "overlay" => self.overlay(rest),
            "undo" => self.undo_layout(),
            "tab" => self.tab(rest),
            "tab-select" => self.tab_select(rest),
            "tab-next" => self.tab_cycle(true),
            "tab-prev" => self.tab_cycle(false),
            "tag-view" => self.tag_view(rest),
            "window-tag" => self.window_tag(rest),
            "tag-remove" => self.tag_remove(rest),
            "keys" => self.keys(),
            "reload" => self.reload(),
            "save" => {
                self.save()?;
                Ok(format!("saved {}", self.layout_path.display()))
            }
            "stop" => {
                self.running = false;
                Ok("stopping".to_string())
            }
            "split-terminal" => self.split_terminal(),
            "palette" => {
                crate::palette::open(&self.socket_path)?;
                Ok("palette starting".to_string())
            }
            "settings" => Ok("settings are in ~/.config/pergola/config.json".to_string()),
            "url" => {
                let raw = rest.join(" ");
                let cmd = Command::parse(&raw).map_err(|e| anyhow!(e))?;
                self.camera_command(cmd)
            }
            other => {
                let cmd = parse_command(other, rest)?;
                self.camera_command(cmd)
            }
        }
    }

    fn camera_command(&mut self, cmd: Command) -> Result<String> {
        let screen = self.x.workarea_rect()?.to_rect();
        let focused = self.focused_panel();
        let action = cmd
            .camera_action(focused.as_deref(), screen)
            .ok_or_else(|| anyhow!("'{}' is not a camera command", cmd.name()))?;
        let camera = Camera::new(self.engine);
        self.camera = camera.apply(&action, &self.camera, &self.file.doc.root, screen);
        match action {
            CameraAction::Everything => self.overlay_on = true,
            CameraAction::ZoomIn { .. } | CameraAction::Frame { .. } => self.overlay_on = false,
            _ => {}
        }
        self.apply()?;
        self.save()?;
        Ok(format!(
            "{} → framed {} (depth {})",
            cmd.name(),
            self.camera.framed,
            self.file.doc.root.nesting_depth()
        ))
    }

    fn status(&self) -> Result<String> {
        let screen = self.x.workarea_rect()?;
        // Count what is on screen: the active tag's view, not the whole tree.
        let shown = self.file.doc.active_tree();
        let panels = shown
            .flatten()
            .iter()
            .filter(|n| matches!(n, LayoutNode::Panel(_)))
            .count();
        // Views beyond the first of each panel are tabs: mapped at the same
        // rectangle, only one of them raised.
        let tabs: usize = shown
            .flatten()
            .iter()
            .filter_map(|n| match n {
                LayoutNode::Panel(p) => Some(p.views.len().saturating_sub(1)),
                _ => None,
            })
            .sum();
        let tab_note = if tabs > 0 {
            format!(" · {tabs} tabs")
        } else {
            String::new()
        };
        // A hint is state worth reporting: it proves the pointer affordance
        // is up (or that it is not left behind) without a screen capture.
        let hint = match self.hint {
            Hint::None => "",
            Hint::Hover(_) => " · hint hover",
            Hint::Drop => " · hint drop",
        };
        // A named workspace is state worth reporting: it says why the screen
        // shows fewer windows than the document holds.
        let tag = match self.file.doc.active_tag.as_deref() {
            Some(name) => format!(" · tag {name}"),
            None => String::new(),
        };
        Ok(format!(
            "pergola {} · {}x{} · {} panels, {} floating{tab_note} · depth {} · framed {} · gap {} · overlay {}{tag}{hint}",
            pergola_core::VERSION,
            screen.w,
            screen.h,
            panels,
            self.file.doc.floating.len(),
            shown.nesting_depth(),
            self.camera.framed,
            self.engine.gap,
            if self.overlay_on { "on" } else { "off" }
        ) + &match self.x.active_window() {
            Some(w) => format!(
                " · active 0x{w:x} ({})",
                self.seat_for_window(w).map(|s| s.panel.as_str()).unwrap_or("unmanaged")
            ),
            None => " · active none".to_string(),
        })
    }

    /// Current geometry: every panel's *frame* rectangle after the camera
    /// transform. Handy for scripts and for checking the daemon against itself.
    fn rects(&self, args: &[String]) -> Result<String> {
        let screen = self.x.workarea_rect()?.to_rect();
        let shown = self.file.doc.active_tree();
        let rects = self.engine.rects(&shown, screen);
        let framed = if self.camera.framed != shown.id() {
            rects
                .iter()
                .find(|(id, _)| id == &self.camera.framed)
                .map(|(_, r)| *r)
        } else {
            None
        };
        if std::env::var_os("PERGOLA_DEBUG").is_some() {
            eprintln!(
                "pergola: rects shown={} framed={:?} screen={:?} raw={:?}",
                shown.id(),
                framed,
                screen,
                rects
            );
        }
        let rows: Vec<serde_json::Value> = rects
            .iter()
            .filter(|(id, _)| {
                shown
                    .node(id)
                    .map(|n| matches!(n, LayoutNode::Panel(_)))
                    .unwrap_or(false)
            })
            .map(|(id, r)| {
                let f = transform(*r, framed, screen).to_irect();
                serde_json::json!({"id": id, "x": f.x, "y": f.y, "w": f.w, "h": f.h})
            })
            .collect();
        if args.iter().any(|a| a == "--json") {
            return Ok(serde_json::to_string_pretty(&rows)?);
        }
        let mut out = String::new();
        for row in rows {
            out.push_str(&format!(
                "{} {} {} {} {}\n",
                row["id"].as_str().unwrap_or(""),
                row["x"],
                row["y"],
                row["w"],
                row["h"]
            ));
        }
        Ok(out.trim_end().to_string())
    }

    fn list(&self) -> Result<String> {
        let windows = self.x.windows()?;
        let mut out = String::new();
        for w in &windows {
            let seat = self.seat_for_window(w.id);
            let where_ = match seat {
                // A view of a tag that is not active is in the tree but off
                // the screen: say "parked" rather than "docked", which would
                // claim a tile the user cannot see.
                Some(s) if !self.file.doc.view_visible(&s.seat) => "parked",
                Some(s) if self.file.doc.root.node(&s.panel).is_some() => {
                    // A view of a tab group that is not the selected one is
                    // mapped at the tile but covered: report it as a tab, not
                    // as a tile someone is looking at.
                    let hidden = match self.file.doc.root.node(&s.panel) {
                        Some(LayoutNode::Panel(p)) => p.visible() != Some(s.seat.as_str()),
                        _ => false,
                    };
                    if hidden { "tab" } else { "docked" }
                }
                Some(_) => "floating",
                None => "unmanaged",
            };
            let tag_note = seat
                .and_then(|s| self.file.doc.tag_of(&s.seat))
                .map(|name| format!("  [tag {name}]"))
                .unwrap_or_default();
            out.push_str(&format!("{:<9} {}{tag_note}\n", where_, describe(w)));
        }
        // Minimised windows are unmapped, so `windows()` does not list them,
        // but their seats are still in the tree. Say so instead of letting the
        // user think the tile vanished.
        for seat in &self.seats {
            if !windows.iter().any(|w| w.id == seat.window) {
                out.push_str(&format!("{:<9} 0x{:08x}  {}\n", "hidden", seat.window, seat.title));
            }
        }
        Ok(out.trim_end().to_string())
    }

    fn dock(&mut self, args: &[String]) -> Result<String> {
        let edge = args
            .first()
            .and_then(|s| Direction::parse(s))
            .ok_or_else(|| anyhow!("usage: pergola dock <left|right|up|down> [--window ID] [--target ID]"))?;
        let win = flag(args, "--window")
            .and_then(|v| parse_window(&v))
            .or_else(|| self.x.active_window())
            .ok_or_else(|| anyhow!("no active window to dock"))?;
        let target = flag(args, "--target");
        self.dock_window(win, edge, target)
    }

    /// Dock one window against `target` (or the last focused space). Split out
    /// from [`Self::dock`] so the pointer path — dropping a window on a screen
    /// edge — docks exactly the same way the CLI does.
    fn dock_window(
        &mut self,
        win: u32,
        edge: Direction,
        target: Option<String>,
    ) -> Result<String> {
        let seat = self
            .seat_for_window(win)
            .ok_or_else(|| anyhow!("window 0x{win:x} is not managed by pergola"))?
            .clone();
        let mut target = target
            .or_else(|| self.focused_panel())
            .unwrap_or_else(|| self.file.doc.root.id().to_string());
        if target == seat.panel {
            // The focused window is the one being docked: dock beside whatever
            // had focus before it, falling back to the whole tree.
            target = self
                .last_focus
                .clone()
                .filter(|p| p != &seat.panel && self.file.doc.root.node(p).is_some())
                .unwrap_or_else(|| self.file.doc.root.id().to_string());
        }
        self.detach_seat(&seat)?;
        let panel_id = self.own_panel_id(&seat);
        if self.file.doc.root.node(&target).is_none() {
            target = self.file.doc.root.id().to_string();
        }
        let node = LayoutNode::panel(panel_id.clone(), vec![seat.seat.clone()]);
        let tree = self
            .engine
            .inserting(node, &target, edge, &self.file.doc.root)
            .map_err(|e| anyhow!("{e}"))?;
        self.replace_root(tree);
        self.normalize_camera();
        self.apply()?;
        self.save()?;
        Ok(format!(
            "docked {} {} of {} (depth {})",
            panel_id,
            match edge {
                Direction::Left => "left",
                Direction::Right => "right",
                Direction::Up => "above",
                Direction::Down => "below",
            },
            target,
            self.file.doc.root.nesting_depth()
        ))
    }

    fn float(&mut self, args: &[String]) -> Result<String> {
        let win = flag(args, "--window")
            .and_then(|v| parse_window(&v))
            .or_else(|| self.x.active_window())
            .ok_or_else(|| anyhow!("no active window to float"))?;
        self.float_window(win, None)
    }

    /// Return one window to the desktop layer. `dropped` is the world-space
    /// rectangle it was let go at (mouse path); without it the window keeps the
    /// rectangle its tile had, so `pergola float` does not teleport it.
    fn float_window(&mut self, win: u32, dropped: Option<Rect>) -> Result<String> {
        let seat = self
            .seat_for_window(win)
            .ok_or_else(|| anyhow!("window 0x{win:x} is not managed by pergola"))?
            .clone();
        let rect = dropped
            // Already floating: keep the rectangle the user put it in. Falling
            // back to the default 80x80x800x500 here teleports the window to
            // the top-left corner, where a WM's own "drag to the edge" gesture
            // then fires (measured 2026-11-15: `float` on an already-floating
            // xterm under openbox jumped it to the top edge and openbox
            // maximised it).
            .or_else(|| {
                self.file
                    .doc
                    .floating
                    .iter()
                    .find(|f| f.panel.id == seat.panel)
                    .map(|f| f.rect)
            })
            .or_else(|| {
                self.engine
                    .rect_of(
                        &self.file.doc.root,
                        self.x.workarea_rect().ok()?.to_rect(),
                        &seat.panel,
                    )
            })
            .unwrap_or_else(|| Rect::new(80.0, 80.0, 800.0, 500.0));
        self.detach_seat(&seat)?;
        let panel_id = self.own_panel_id(&seat);
        self.file.doc.floating.push(FloatingWindow {
            panel: pergola_core::Panel::new(panel_id.clone(), vec![seat.seat.clone()]),
            rect,
            size: pergola_core::Size::new(rect.w as i32, rect.h as i32),
            z: None,
        });
        self.place_floating = true;
        self.normalize_camera();
        self.apply()?;
        self.save()?;
        Ok(format!("floated {panel_id}"))
    }

    /// Move a window into a panel as a tab. Without `--target` the window joins
    /// the panel that had focus before it — so `pergola tab` on the active
    /// window folds it into the neighbouring panel instead of doing nothing.
    /// A panel that gains a second view becomes a tab group; only its selected
    /// view is raised.
    fn tab(&mut self, args: &[String]) -> Result<String> {
        let win = flag(args, "--window")
            .and_then(|v| parse_window(&v))
            .or_else(|| self.x.active_window())
            .ok_or_else(|| anyhow!("no active window to tab"))?;
        let seat = self
            .seat_for_window(win)
            .ok_or_else(|| anyhow!("window 0x{win:x} is not managed by pergola"))?
            .clone();
        // A seat already in a panel is a tab move; a floating seat is folded in
        // from the desktop layer. `panel_containing` is None for the latter and
        // `detach_seat` removes it from the floating layer, so the same code
        // folds both into the target panel.
        let source = self.file.doc.root.panel_containing(&seat.seat).cloned();
        let source_id = source.as_ref().map(|p| p.id.clone());
        let target = match flag(args, "--target") {
            Some(t) => t,
            None => match self.focused_panel() {
                Some(p) if Some(&p) != source_id.as_ref() && self.file.doc.root.node(&p).is_some() => {
                    p
                }
                _ => self
                    .last_focus
                    .clone()
                    .filter(|p| {
                        Some(p) != source_id.as_ref() && self.file.doc.root.node(p).is_some()
                    })
                    .or_else(|| {
                        // A floating window has no panel of its own and may have
                        // no last-focused tile; fall back to the first leaf so the
                        // command folds it somewhere sensible instead of failing.
                        self.file
                            .doc
                            .root
                            .leaves()
                            .into_iter()
                            .find(|n| !n.is_desktop())
                            .map(|n| n.id().to_string())
                    })
                    .unwrap_or_else(|| self.file.doc.root.id().to_string()),
            },
        };
        let target_id = if self.file.doc.root.node(&target).is_some() {
            target
        } else {
            self.file
                .doc
                .root
                .panel_containing(&target)
                .map(|p| p.id.clone())
                .ok_or_else(|| anyhow!("no panel '{target}' to tab into"))?
        };
        if Some(&target_id) == source_id.as_ref() {
            // Already in the target panel: show it rather than rebuilding.
            let Some(mut panel) = source else {
                return Err(anyhow!("no panel '{target_id}' to tab into"));
            };
            panel.select(&seat.seat);
            let tree = replace_node(&self.file.doc.root, &target_id, &LayoutNode::Panel(panel));
            self.replace_root(tree);
            self.apply()?;
            self.save()?;
            return Ok(format!("showing {} in {}", seat.seat, target_id));
        }
        // Take the seat out of its old panel. A tab group loses one view; a
        // plain panel goes away like any other node.
        self.detach_seat(&seat)?;
        let Some(LayoutNode::Panel(mut target_panel)) =
            self.file.doc.root.node(&target_id).cloned()
        else {
            return Err(anyhow!("no panel '{target_id}' to tab into"));
        };
        // The target's existing views define the workspace of the tab group.
        // Reading this after appending the source could mistake the source's
        // old tag for the target's when the target is untagged.
        let target_tag = target_panel
            .views
            .iter()
            .find_map(|v| self.file.doc.tag_of(v).map(str::to_string));
        if !target_panel.views.iter().any(|v| v == &seat.seat) {
            target_panel.views.push(seat.seat.clone());
        }
        target_panel.select(&seat.seat);
        target_panel.tabbed = Some(true);
        let views = target_panel.views.len();
        let tree = replace_node(&self.file.doc.root, &target_id, &LayoutNode::Panel(target_panel));
        self.replace_root(tree);
        // The seat now belongs to the target panel; without this `focused_panel`
        // and `list` keep pointing at the panel it just left.
        if let Some(record) = self.seats.iter_mut().find(|s| s.seat == seat.seat) {
            record.panel = target_id.clone();
        }
        // The window now lives inside the target panel, so it belongs to that
        // panel's workspace. A view on tag A inside a tile on tag B would be
        // pruned out of the very panel it was folded into.
        if let Some(tag) = target_tag {
            self.file.doc.assign_view(&seat.seat, &tag);
        } else {
            self.file.doc.forget_view(&seat.seat);
        }
        self.normalize_camera();
        self.apply()?;
        // The tabbed window is the one being shown, so give it the keyboard.
        self.focus_seat(&seat.seat);
        self.save()?;
        Ok(format!(
            "tabbed {} into {target_id} ({views} views)",
            seat.seat
        ))
    }

    /// Show one view of a tab group. Takes a seat id or a window id.
    fn tab_select(&mut self, args: &[String]) -> Result<String> {
        let raw = args
            .first()
            .ok_or_else(|| anyhow!("usage: pergola tab-select <seat|window>"))?;
        let seat = match self.seats.iter().find(|s| &s.seat == raw) {
            Some(s) => s.seat.clone(),
            None => {
                let win = parse_window(raw)
                    .ok_or_else(|| anyhow!("'{raw}' is not a seat or window id"))?;
                self.seat_for_window(win)
                    .ok_or_else(|| anyhow!("window 0x{win:x} is not managed by pergola"))?
                    .seat
                    .clone()
            }
        };
        let panel = self
            .file
            .doc
            .root
            .panel_containing(&seat)
            .cloned()
            .ok_or_else(|| anyhow!("{seat} is not in the layout"))?;
        if !panel.is_tab_group() {
            return Ok(format!("{} is not a tab group", panel.id));
        }
        let panel_id = panel.id.clone();
        let mut panel = panel;
        if !panel.select(&seat) {
            return Err(anyhow!("{seat} is not a view of {panel_id}"));
        }
        let tree = replace_node(&self.file.doc.root, &panel_id, &LayoutNode::Panel(panel));
        self.replace_root(tree);
        self.apply()?;
        self.focus_seat(&seat);
        self.save()?;
        Ok(format!("showing {seat} in {panel_id}"))
    }

    /// Cycle the visible view of the focused tab group.
    fn tab_cycle(&mut self, forward: bool) -> Result<String> {
        // The group to cycle is the one the user is looking at. That is the
        // panel of the active window when it is a tab group; when the WM has
        // parked focus elsewhere (a new window, a dialog) fall back to the
        // group focused last. Both are resolved from the tree, never from the
        // seat record, which can lag a move.
        let mut panel_id = self
            .focused_panel()
            .filter(|id| self.is_tab_group(id))
            .or_else(|| {
                self.last_focus
                    .clone()
                    .filter(|id| self.is_tab_group(id))
            });
        if panel_id.is_none() {
            // Last resort: any tab group at all. A user pressing tab-next with
            // one group on screen means that group.
            panel_id = self
                .file
                .doc
                .root
                .flatten()
                .into_iter()
                .find_map(|n| match n {
                    LayoutNode::Panel(p) if p.is_tab_group() => Some(p.id.clone()),
                    _ => None,
                });
        }
        let Some(panel_id) = panel_id else {
            return Ok("no tab group to cycle".to_string());
        };
        let Some(LayoutNode::Panel(mut panel)) = self.file.doc.root.node(&panel_id).cloned() else {
            return Ok(format!("{panel_id} is not a panel"));
        };
        let Some(seat) = panel.cycle(forward) else {
            return Ok(format!("{panel_id} has no views"));
        };
        let tree = replace_node(&self.file.doc.root, &panel_id, &LayoutNode::Panel(panel));
        self.replace_root(tree);
        self.apply()?;
        self.focus_seat(&seat);
        self.save()?;
        Ok(format!("showing {seat} in {panel_id}"))
    }

    /// Whether a panel id names a tab group in the tree.
    fn is_tab_group(&self, id: &str) -> bool {
        matches!(
            self.file.doc.root.node(id),
            Some(LayoutNode::Panel(p)) if p.is_tab_group()
        )
    }

    /// Give the keyboard to a seat's window. Switching tabs without this leaves
    /// the user typing into the hidden view, which is the bug the whole feature
    /// exists to avoid.
    fn focus_seat(&mut self, seat: &str) {
        if let Some(window) = self
            .seats
            .iter()
            .find(|s| s.seat == seat)
            .map(|s| s.window)
        {
            self.activate_with_guard(window);
        }
    }

    /// Ask the desktop to focus `window`, remembering that we did. The request
    /// is broadcast to every client selecting root's substructure events —
    /// including us — so `note_client_message` must be able to tell our own
    /// echo from a pager's request.
    fn activate_with_guard(&mut self, window: u32) {
        let now = Instant::now();
        self.own_activations
            .retain(|(_, at)| now.duration_since(*at) < ACTIVATION_ECHO);
        if let Err(e) = self.x.activate(window) {
            // No request went out, so no echo is coming: an entry here would
            // swallow a genuine pager request for this window for 2 s.
            eprintln!("pergola: could not focus 0x{window:x}: {e}");
            return;
        }
        self.own_activations.push((window, now));
        // Bounded: an old request whose echo never arrived must not accumulate.
        if self.own_activations.len() > ACTIVATION_ECHO_DEPTH {
            self.own_activations.remove(0);
        }
    }

    /// Handle an EWMH `_NET_ACTIVE_WINDOW` request. The window to activate is
    /// the message's event window; `data[0]` is the source indication (2 =
    /// pager, 1 = application) and `data[1]` the timestamp, which clients often
    /// send as 0 — so it is never used to reject a request.
    fn note_client_message(&mut self, message: &ClientMessageEvent) -> Result<()> {
        if message.format != 32 || message.type_ != self.x.net_active_window_atom() {
            return Ok(());
        }
        let source = message.data.as_data32().first().copied().unwrap_or(0);
        // Our own activation echoing back: consume it so focus cannot loop.
        if take_own_activation(&mut self.own_activations, message.window, Instant::now()) {
            return Ok(());
        }
        let Some((seat, panel, hidden)) =
            activation_target(&self.seats, &self.file.doc.root, message.window)
        else {
            // Unmanaged, override-redirect or parked: a pager's stale entry or
            // a popup, neither of which belongs in the tree (xmonad's
            // EwmhDesktops drops them for the same reason).
            if std::env::var_os("PERGOLA_DEBUG").is_some() {
                eprintln!(
                    "pergola: activation 0x{:x} source {source}: not managed, ignored",
                    message.window
                );
            }
            return Ok(());
        };
        if hidden {
            // A tab view that is not shown has to be shown first, or the
            // desktop focuses a window nobody can see.
            if let Some(LayoutNode::Panel(mut node)) = self.file.doc.root.node(&panel).cloned() {
                if node.select(&seat) {
                    let tree =
                        replace_node(&self.file.doc.root, &panel, &LayoutNode::Panel(node));
                    self.replace_root(tree);
                    self.normalize_camera();
                    self.apply()?;
                    self.save()?;
                }
            }
        }
        // A positive line, not just the failure one: the e2e needs to prove the
        // daemon *handled* the request, and `status` cannot show it because the
        // desktop sets `_NET_ACTIVE_WINDOW` whether or not a handler exists.
        if std::env::var_os("PERGOLA_DEBUG").is_some() {
            eprintln!(
                "pergola: activation 0x{:x} source {source} -> seat {seat} (panel {panel}, hidden {hidden})",
                message.window
            );
        }
        if self.file.doc.root.node(&panel).is_some() {
            self.last_focus = Some(panel);
        }
        self.activate_with_guard(message.window);
        Ok(())
    }

    fn arrange(&mut self) -> Result<String> {
        let mut panels: Vec<LayoutNode> = Vec::new();
        for node in self.file.doc.root.flatten() {
            if let LayoutNode::Panel(p) = node {
                panels.push(LayoutNode::Panel(p.clone()));
            }
        }
        for floating in &self.file.doc.floating {
            panels.push(LayoutNode::Panel(floating.panel.clone()));
        }
        if panels.is_empty() {
            return Ok("nothing to arrange".to_string());
        }
        let count = panels.len();
        let tree = self.engine.arrange(panels, self.config.desktop_share);
        self.replace_root(tree);
        self.file.doc.floating.clear();
        self.normalize_camera();
        self.apply()?;
        self.save()?;
        Ok(format!(
            "arranged {count} panels (depth {})",
            self.file.doc.root.nesting_depth()
        ))
    }

    fn float_all(&mut self) -> Result<String> {
        let screen = self.x.workarea_rect()?.to_rect();
        let rects = self.engine.rects(&self.file.doc.root, screen);
        let mut moved = 0;
        for node in self.file.doc.root.flatten() {
            if let LayoutNode::Panel(p) = node {
                let rect = rects
                    .iter()
                    .find(|(id, _)| id == &p.id)
                    .map(|(_, r)| *r)
                    .unwrap_or_else(|| Rect::new(80.0, 80.0, 800.0, 500.0));
                self.file.doc.floating.push(FloatingWindow {
                    panel: p.clone(),
                    rect,
                    size: pergola_core::Size::new(rect.w as i32, rect.h as i32),
                    z: None,
                });
                moved += 1;
            }
        }
        self.replace_root(LayoutNode::desktop());
        self.camera.framed = "desktop".to_string();
        self.place_floating = true;
        self.normalize_camera();
        self.apply()?;
        self.save()?;
        Ok(format!("floated {moved} panels"))
    }

    fn set_gap(&mut self, args: &[String]) -> Result<String> {
        let gap: f64 = args
            .first()
            .ok_or_else(|| anyhow!("usage: pergola gap <pixels>"))?
            .parse()
            .map_err(|_| anyhow!("gap must be a number"))?;
        self.engine = LayoutEngine::new(gap, self.engine.minimum_share)
            .with_desktop_share(self.engine.desktop_share);
        self.config.gap = gap;
        self.apply()?;
        self.save()?;
        Ok(format!("gap {gap}"))
    }

    /// Rebalance the split that contains the focused panel: `ratio 0.3` gives
    /// the focused window 30 % of its split. The siblings keep their relative
    /// proportions, so a two-child split moves the boundary to exactly 30 % —
    /// the same result as dragging the gap by the same amount.
    fn set_ratio(&mut self, args: &[String]) -> Result<String> {
        let text = args.first().ok_or_else(|| {
            anyhow!("usage: pergola ratio <0.0-1.0> | +delta | -delta")
        })?;
        // A leading sign means "nudge by this much", which is what a chord can
        // do: the same key widens or narrows whatever split has focus, without
        // knowing its current size. `+0.05` and `-0.05` are parsed as numbers.
        let relative = text.starts_with('+') || text.starts_with('-');
        let amount: f64 = text.parse().map_err(|_| anyhow!("ratio must be a number"))?;
        let focused = self
            .focused_panel()
            .ok_or_else(|| anyhow!("no focused window to rebalance"))?;
        let ratio = if relative {
            let parent = self
                .file
                .doc
                .root
                .parent(&focused)
                .ok_or_else(|| anyhow!("'{focused}' has no split to rebalance"))?;
            let index = parent
                .children
                .iter()
                .position(|child| child.id() == focused)
                .ok_or_else(|| anyhow!("'{focused}' is not in its parent split"))?;
            parent.weights.get(index).copied().unwrap_or(0.0) + amount
        } else {
            amount
        };
        if !(0.0..=1.0).contains(&ratio) {
            return Err(anyhow!("ratio must be between 0.0 and 1.0"));
        }
        let tree = self
            .engine
            .set_ratio(&focused, ratio, &self.file.doc.root)
            .map_err(|e| anyhow!("{e}"))?;
        self.replace_root(tree);
        self.normalize_camera();
        self.apply()?;
        self.save()?;
        // Report the resulting weights so the change is observable without a UI.
        // The applied share is the focused panel's own weight, which is what the
        // engine clamped — not necessarily the number that was asked for.
        let parent = self.file.doc.root.parent(&focused);
        let weights = parent.map(|s| s.weights.clone()).unwrap_or_default();
        let applied = parent
            .and_then(|s| {
                s.children
                    .iter()
                    .position(|child| child.id() == focused)
                    .and_then(|index| s.weights.get(index).copied())
            })
            .unwrap_or(ratio);
        let shown: Vec<String> = weights.iter().map(|w| format!("{w:.2}")).collect();
        Ok(format!(
            "ratio {applied:.2} · split weights [{}]",
            shown.join(", ")
        ))
    }

    // MARK: named workspaces

    /// Switch to a tag, or list the tags when no name is given.
    ///
    /// `all` (or `off`) clears the active tag so every view is shown again: the
    /// escape hatch from a workspace that seems to have swallowed the window
    /// the user wanted. A tag that does not exist yet is created empty, so
    /// `tag-view work` is how a workspace is started.
    fn tag_view(&mut self, args: &[String]) -> Result<String> {
        let Some(name) = args.first().map(|s| s.as_str()) else {
            return Ok(self.tag_report());
        };
        // Remember the keyboard position on the tag being left, so the return
        // trip lands on the window the user was working in, not on whichever
        // window the desktop happens to focus last.
        let focused = self
            .x
            .active_window()
            .and_then(|w| self.seat_for_window(w).map(|s| s.seat.clone()));
        if let Some(current) = self.file.doc.active_tag.clone() {
            if let Some(tag) = self.file.doc.tag_mut(&current) {
                tag.focus = focused;
            }
        }
        let clearing = name == "all" || name == "off" || name == "*";
        if clearing {
            self.file.doc.active_tag = None;
        } else {
            self.file.doc.ensure_tag(name);
            self.file.doc.active_tag = Some(name.to_string());
        }
        self.normalize_camera();
        // Entering a tag must put its floating windows back where the user left
        // them; they were parked below the screen while the tag was away.
        self.place_floating = true;
        self.apply()?;
        // Restore the keyboard on the tag just entered, before saving, so the
        // next `tag-view` remembers the right place.
        let restore = if clearing {
            None
        } else {
            self.file
                .doc
                .tag(name)
                .and_then(|t| t.focus.clone())
                .filter(|seat| self.file.doc.view_visible(seat))
        };
        if let Some(seat) = restore {
            if self.seats.iter().any(|s| s.seat == seat) {
                self.focus_seat(&seat);
            }
        }
        self.save()?;
        let message = match self.file.doc.active_tag.clone() {
            Some(name) => {
                let views = self
                    .file
                    .doc
                    .tag(&name)
                    .map(|t| t.views.len())
                    .unwrap_or(0);
                if views == 0 {
                    // A new tag is empty on purpose, but an empty screen with no
                    // explanation reads as a crash. Name the way back.
                    format!("tag {name} is empty — `pergola window-tag {name}` adds a window, `tag-view all` shows everything")
                } else {
                    format!(
                        "tag {name} ({} view{})",
                        views,
                        if views == 1 { "" } else { "s" }
                    )
                }
            }
            None => "tag off — every view is shown".to_string(),
        };
        // Switching workspaces changes the whole screen at once, and the screen
        // itself says nothing about where you are. The desktop's own notifier
        // names the workspace: a switch is a deliberate keypress, so this is the
        // one moment the words are worth the pixels.
        crate::notify::send(&message);
        Ok(message)
    }

    /// Move a window into a tag without switching to it. The tag is created
    /// when it does not exist yet, so this is also how a workspace is started:
    /// `pergola window-tag work` on each window you want there.
    fn window_tag(&mut self, args: &[String]) -> Result<String> {
        let Some(name) = args.first() else {
            return Err(anyhow!(
                "usage: pergola window-tag <name> [--window ID]"
            ));
        };
        if name == "all" || name == "off" || name == "*" {
            return Err(anyhow!("'{name}' is not a tag name"));
        }
        let win = flag(args, "--window")
            .and_then(|v| parse_window(&v))
            .or_else(|| self.x.active_window())
            .ok_or_else(|| anyhow!("no active window to tag"))?;
        let seat = self
            .seat_for_window(win)
            .ok_or_else(|| anyhow!("window 0x{win:x} is not managed by pergola"))?
            .clone();
        self.file.doc.assign_view(&seat.seat, name);
        // The window may be leaving the screen (it went to another tag) or
        // coming back to it: let `apply` park or place it either way.
        self.place_floating = true;
        self.apply()?;
        self.save()?;
        Ok(format!(
            "moved {} to tag {name}{}",
            seat.panel,
            if self.file.doc.view_visible(&seat.seat) {
                ""
            } else {
                " (off screen until you switch to it)"
            }
        ))
    }

    /// Delete a workspace. Its views become untagged, so they show again under
    /// `tag-view all` — the workspace name goes, the windows do not. This is the
    /// way out of a tag that a closed window left behind.
    fn tag_remove(&mut self, args: &[String]) -> Result<String> {
        let Some(name) = args.first() else {
            return Err(anyhow!("usage: pergola tag-remove <name>"));
        };
        let Some(index) = self.file.doc.tags.iter().position(|t| &t.name == name) else {
            return Err(anyhow!("no tag '{name}'"));
        };
        let tag = self.file.doc.tags.remove(index);
        let was_active = self.file.doc.active_tag.as_deref() == Some(tag.name.as_str());
        if was_active {
            // Showing a tag that no longer exists would hide every view.
            self.file.doc.active_tag = None;
            self.normalize_camera();
            self.place_floating = true;
            self.apply()?;
        }
        self.save()?;
        Ok(format!(
            "removed tag {name}{} ({} view{} now untagged)",
            if was_active { " — every view is shown" } else { "" },
            tag.views.len(),
            if tag.views.len() == 1 { "" } else { "s" }
        ))
    }

    /// One line per tag: a `*` marks the one on screen, and the view count says
    /// how many windows it owns.
    fn tag_report(&self) -> String {
        if self.file.doc.tags.is_empty() {
            return "no tags — every view is shown\n  start one with: pergola window-tag work"
                .to_string();
        }
        let mut out = String::new();
        for tag in &self.file.doc.tags {
            let marker = if self.file.doc.active_tag.as_deref() == Some(tag.name.as_str()) {
                "*"
            } else {
                " "
            };
            out.push_str(&format!(
                "{marker} {:<12} {} view{}\n",
                tag.name,
                tag.views.len(),
                if tag.views.len() == 1 { "" } else { "s" }
            ));
        }
        out.trim_end().to_string()
    }

    fn overlay(&mut self, args: &[String]) -> Result<String> {
        match args.first().map(String::as_str) {
            Some("on") => self.overlay_on = true,
            Some("off") => self.overlay_on = false,
            _ => self.overlay_on = !self.overlay_on,
        }
        self.apply()?;
        self.save()?;
        let dim = match (self.overlay_on, self.x.overlay_dim()) {
            (false, _) => String::new(),
            (true, Some(id)) => format!(" · dim {}% 0x{id:x}", dim_percent(self.config.dim)),
            (true, None) if self.config.dim <= 0.0 => " · dim off (dim 0)".to_string(),
            (true, None) => " · dim unavailable (no compositor or SHAPE)".to_string(),
        };
        Ok(format!(
            "overlay {}{dim}",
            if self.overlay_on { "on" } else { "off" }
        ) + &match self.x.active_window() {
            Some(w) => format!(
                " · active 0x{w:x} ({})",
                self.seat_for_window(w).map(|s| s.panel.as_str()).unwrap_or("unmanaged")
            ),
            None => " · active none".to_string(),
        })
    }

    fn keys(&self) -> Result<String> {
        let mut out = String::new();
        for (binding, command) in &self.bindings {
            out.push_str(&format!("{:<24} {}\n", binding.label, command));
        }
        Ok(out.trim_end().to_string())
    }

    fn reload(&mut self) -> Result<String> {
        // Validate before changing the engine or releasing any key grabs.
        // X11 re-grab failures are reported separately by reinstall_bindings;
        // this guard covers invalid user configuration, not X server failure.
        self.config = Config::checked_for_reload()?;
        self.engine = LayoutEngine::new(self.config.gap, self.config.minimum_share)
            .with_desktop_share(self.config.desktop_share);
        // Re-grab so a `bindings` edit takes effect without a restart. The
        // notes come back in the reply so the caller sees a conflict where it
        // ran the command, not only in the journal.
        let notes = self.reinstall_bindings()?;
        self.apply()?;
        let mut message = "config reloaded".to_string();
        for note in notes {
            message.push_str("\n");
            message.push_str(&note);
        }
        Ok(message)
    }

    fn split_terminal(&mut self) -> Result<String> {
        // Remember the source before the new terminal takes focus. Reading
        // _NET_ACTIVE_WINDOW after spawning can name the new floating window,
        // whose panel disappears when it is moved into the tree.
        let source_window = self
            .x
            .active_window()
            .ok_or_else(|| anyhow!("focus a window before splitting with a terminal"))?;
        let source = self
            .seat_for_window(source_window)
            .ok_or_else(|| anyhow!("the focused window is not managed by pergola"))?
            .clone();
        let before: HashSet<u32> = self.x.client_list()?.into_iter().collect();
        let mut command = std::process::Command::new(&self.config.terminal);
        if self.config.terminal.ends_with("xfce4-terminal") {
            command.arg("--disable-server");
        }
        command
            .spawn()
            .with_context(|| {
                format!(
                    "cannot launch {}; set `terminal` in ~/.config/pergola/config.json to an installed terminal",
                    self.config.terminal
                )
            })?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut new_window = None;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(120));
            if let Ok(list) = self.x.client_list() {
                if let Some(id) = list.into_iter().find(|id| !before.contains(id)) {
                    new_window = Some(id);
                    break;
                }
            }
        }
        let win = new_window.ok_or_else(|| anyhow!("the terminal did not open a window"))?;
        self.reconcile()?;
        self.normalize_camera();
        let seat = self
            .seat_for_window(win)
            .ok_or_else(|| anyhow!("new terminal window was not adopted"))?
            .clone();
        // Build and validate both tile changes before touching the saved
        // document. This also handles an auto-docked new window: remove its
        // provisional placement, then put it beside the original source.
        let mut tree = self.file.doc.root.clone();
        if tree.panel_containing(&seat.seat).is_some() {
            tree = self
                .engine
                .removing_view(&seat.seat, &tree)
                .map_err(|e| anyhow!("{e}"))?;
        }
        let source_was_floating = tree.panel_containing(&source.seat).is_none();
        if source_was_floating {
            let anchor = self
                .last_focus
                .as_ref()
                .filter(|id| tree.node(id).is_some())
                .cloned()
                .unwrap_or_else(|| tree.id().to_string());
            let node = LayoutNode::panel(source.panel.clone(), vec![source.seat.clone()]);
            tree = self
                .engine
                .inserting(node, &anchor, Direction::Left, &tree)
                .map_err(|e| anyhow!("{e}"))?;
        }
        let target = tree
            .panel_containing(&source.seat)
            .ok_or_else(|| anyhow!("the focused window has no tile to split"))?
            .id
            .clone();
        let node = LayoutNode::panel(seat.panel.clone(), vec![seat.seat.clone()]);
        tree = self
            .engine
            .inserting(node, &target, Direction::Right, &tree)
            .map_err(|e| anyhow!("{e}"))?;
        self.file.doc.floating.retain(|item| {
            !item.panel.views.contains(&seat.seat)
                && (!source_was_floating || !item.panel.views.contains(&source.seat))
        });
        self.replace_root(tree);
        self.normalize_camera();
        self.apply()?;
        self.save()?;
        Ok(format!("split with {} (0x{win:x})", self.config.terminal))
    }

    // MARK: event stream (`pergola subscribe`)

    /// Register a status-bar client. The first record is the committed state,
    /// so a bar that connects after startup does not have to poll for what it
    /// missed (bspwm's startup-state gap).
    fn subscribe_stream(
        &mut self,
        stream: &UnixStream,
        before: &mut Option<EventSig>,
    ) -> Result<()> {
        if self.subscribers.len() >= SUB_MAX_CLIENTS {
            anyhow::bail!("too many subscribers (limit {SUB_MAX_CLIENTS})");
        }
        // Keep a duplicate handle so the caller still owns one and can answer a
        // refusal on the same socket; the duplicate shares the socket, so the
        // client cannot tell which descriptor writes.
        let stream = stream.try_clone()?;
        stream.set_nonblocking(true)?;
        self.subscribers.push(Subscriber {
            stream,
            queue: std::collections::VecDeque::new(),
            partial: 0,
            drained: Instant::now(),
        });
        let active = self.x.active_window().unwrap_or(0);
        if before.is_none() {
            // Later requests in this pass can mutate the state we are about to send.
            *before = self.event_sig(Some(active));
        }
        let state = self.state_record(active);
        let last = self.subscribers.len() - 1;
        self.push_record(last, state);
        self.flush_subscribers();
        Ok(())
    }

    fn state_record(&self, active: u32) -> serde_json::Value {
        serde_json::json!({
            "type": "state",
            "tree": serde_json::to_value(&self.file.doc.root).unwrap_or(serde_json::Value::Null),
            "gap": self.engine.gap,
            "overlay": self.overlay_on,
            "active": active,
            "tag": self.file.doc.active_tag,
            // The full tag list, so a bar can draw every workspace tab from the
            // one snapshot it already reads.
            "tags": self.file.doc.tags,
        })
    }

    fn push_record(&mut self, idx: usize, record: serde_json::Value) {
        let Ok(mut line) = serde_json::to_string(&record) else {
            return;
        };
        line.push('\n');
        let Some(client) = self.subscribers.get_mut(idx) else {
            return;
        };
        // A full queue means the client is not reading. The flush below drops
        // it once the timeout expires; dropping the record instead of growing
        // without bound keeps a stuck bar from taking the daemon down.
        if client.queue.len() < SUB_QUEUE_MAX {
            // A record going into an empty queue restarts the clock: the
            // timeout measures how long *undelivered* data has waited, and a
            // stale `drained` would drop the client on the first WouldBlock.
            if client.queue.is_empty() {
                client.drained = Instant::now();
            }
            client.queue.push_back(line);
        }
    }

    fn publish(&mut self, record: serde_json::Value) {
        for idx in 0..self.subscribers.len() {
            self.push_record(idx, record.clone());
        }
    }

    /// Write what the subscribers can take without blocking. A `WouldBlock` is
    /// not an error: the rest stays queued and the next loop pass retries.
    fn flush_subscribers(&mut self) {
        if self.subscribers.is_empty() {
            return;
        }
        let now = Instant::now();
        self.subscribers.retain_mut(|client| {
            while let Some(line) = client.queue.pop_front() {
                // Slice bytes, not chars: a partial write can end mid-UTF-8, and
                // the next attempt must resume there, not at a char boundary.
                let rest = &line.as_bytes()[client.partial..];
                match client.stream.write(rest) {
                    Ok(0) => return false,
                    Ok(written) => {
                        client.drained = now;
                        client.partial += written;
                        if client.partial < line.len() {
                            client.queue.push_front(line);
                            break;
                        }
                        client.partial = 0;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        client.queue.push_front(line);
                        break;
                    }
                    // A closed peer, or any other write error: forget it.
                    Err(_) => return false,
                }
            }
            // An empty queue means nothing is waiting for the client: it is
            // idle, not stuck. Only a client with undelivered records that has
            // not drained for the whole timeout is gone or wedged.
            client.queue.is_empty() || now.duration_since(client.drained) < SUB_TIMEOUT
        });
    }

    /// The committed state, or `None` when nobody is listening — the diff is
    /// only worth computing for a client that will read it.
    fn event_sig(&self, active: Option<u32>) -> Option<EventSig> {
        if self.subscribers.is_empty() {
            return None;
        }
        let mut placements = std::collections::BTreeMap::new();
        for seat in &self.seats {
            let placement = if self.file.doc.root.node(&seat.panel).is_some() {
                Placement::Docked(seat.panel.clone())
            } else if self.file.doc.floating.iter().any(|f| f.panel.id == seat.panel) {
                Placement::Floating(seat.panel.clone())
            } else {
                // Not in the tree and not floating: a hidden or unmanaged
                // window. It is not part of the streamed state.
                continue;
            };
            placements.insert(seat.window, (placement, seat.title.clone()));
        }
        Some(EventSig {
            root: serde_json::to_string(&self.file.doc.root).ok()?,
            placements,
            active: active.unwrap_or_else(|| self.x.active_window().unwrap_or(0)),
            tag: self.file.doc.active_tag.clone(),
        })
    }

    fn emit_diff(&mut self, before: EventSig, after: &EventSig) {
        for record in diff_events(&before, after) {
            self.publish(record);
        }
        self.flush_subscribers();
    }

    // MARK: event loop

    pub fn run(&mut self, socket: &std::path::Path) -> Result<()> {
        self.socket_path = socket.to_path_buf();
        let socket = socket.to_path_buf();
        if socket.exists() {
            // One daemon per display. A socket that still accepts a connection
            // belongs to a live daemon: refuse instead of unlinking it, which
            // would leave the first daemon running but unreachable.
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                anyhow::bail!(
                    "a pergola daemon is already running on {} (one per display)",
                    socket.display()
                );
            }
            // No listener: a killed daemon left the file behind. Reclaim it.
            let _ = std::fs::remove_file(&socket);
        }
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("binding {}", socket.display()))?;
        let (tx, rx) = mpsc::channel::<(Request, UnixStream)>();
        let (mut wake, mut signal) = UnixStream::pair()?;
        signal.set_nonblocking(true)?;
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let Ok(clone) = stream.try_clone() else { continue };
                let mut reader = BufReader::new(clone);
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() {
                    if let Ok(request) = serde_json::from_str::<Request>(line.trim()) {
                        if tx.send((request, stream)).is_err() {
                            break;
                        }
                        // A full socket already carries a pending wakeup.
                        let _ = signal.write_all(&[1]);
                    }
                }
            }
        });

        // A pager's `_NET_ACTIVE_WINDOW` request arrives only if we select
        // root's substructure events; failure is not fatal, a taskbar click
        // then simply does nothing.
        let watches_root = match self.x.watch_root() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("pergola: cannot watch activation requests: {e:#}");
                false
            }
        };
        match self.reinstall_bindings() {
            Ok(notes) => {
                for note in notes {
                    eprintln!("pergola: {note}");
                }
            }
            Err(e) => eprintln!("pergola: cannot grab hotkeys: {e}"),
        }

        eprintln!(
            "pergola {} listening on {} ({} windows)",
            pergola_core::VERSION,
            socket.display(),
            self.seats.len()
        );

        let mut last_reconcile = Instant::now();
        let mut committed: Option<EventSig> = None;
        let mut focus_deadline: Option<Instant> = None;
        while self.running {
            // One committed snapshot per loop pass: every mutation inside the
            // pass (reconcile, a request, a drag settling) is diffed against
            // the state the previous pass left, so a bar sees transitions, not
            // intermediate half-applied trees (i3#3893's phantom focus).
            let mut before = committed.take().or_else(|| self.event_sig(None));
            let moving = !self.moves.is_empty() || !self.deferred.is_empty();
            let mut timeout = Duration::from_secs(2).saturating_sub(last_reconcile.elapsed());
            if moving || !watches_root || self.subscribers.iter().any(|c| !c.queue.is_empty()) {
                timeout = timeout.min(Duration::from_millis(50));
            }
            if before.is_none() {
                focus_deadline = None;
            }
            if let Some(deadline) = focus_deadline {
                timeout = timeout.min(deadline.saturating_duration_since(Instant::now()));
            }
            let event = wait_for_input(&self.x, &mut wake, timeout);
            if last_reconcile.elapsed() >= Duration::from_secs(2) {
                last_reconcile = Instant::now();
                if let Err(e) = self.reconcile_layout() {
                    eprintln!("pergola: reconcile: {e:#}");
                }
            }
            while let Ok((request, mut stream)) = rx.try_recv() {
                // `subscribe` turns the request/response socket into a stream:
                // the daemon keeps this client and pushes records to it.
                if request.argv.first().map(String::as_str) == Some("subscribe") {
                    if let Err(e) = self.subscribe_stream(&stream, &mut before) {
                        // A refused client must hear why: it is waiting on a
                        // read that would otherwise never return.
                        eprintln!("pergola: subscribe: {e:#}");
                        let response = Response::err(format!("subscribe: {e:#}"));
                        let _ = writeln!(stream, "{}", serde_json::to_string(&response)?);
                    }
                    continue;
                }
                let response = match self.handle(&request.argv) {
                    Ok(message) => Response::ok(message),
                    Err(e) => Response::err(format!("{e:#}")),
                };
                let _ = writeln!(stream, "{}", serde_json::to_string(&response)?);
            }
            let _ = self.settle_moves();
            // One blocking poll for the idle path, then drain whatever else the
            // server has queued (a window drag produces a burst of configures,
            // and the pointer drag stream must not fall behind).
            match event {
                Ok(Some(event)) => {
                    if let Err(e) = self.on_x_event(event) {
                        eprintln!("pergola: event: {e:#}");
                    }
                    let mut drained = 0;
                    while drained < 256 {
                        match self.x.poll_event() {
                            Ok(Some(event)) => {
                                if let Err(e) = self.on_x_event(event) {
                                    eprintln!("pergola: event: {e:#}");
                                }
                                drained += 1;
                            }
                            _ => break,
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("pergola: X connection lost: {e}");
                    break;
                }
            }
            if let Some(before) = before {
                if let Some(mut after) = self.event_sig(None) {
                    // Some WMs briefly clear _NET_ACTIVE_WINDOW between two
                    // clients. Publish a genuine focus loss after one short
                    // settling interval, without delaying other stream records.
                    if before.active != 0 && after.active == 0 {
                        let deadline = focus_deadline.get_or_insert_with(|| {
                            Instant::now() + Duration::from_millis(50)
                        });
                        if Instant::now() < *deadline {
                            after.active = before.active;
                        } else {
                            focus_deadline = None;
                        }
                    } else {
                        focus_deadline = None;
                    }
                    self.emit_diff(before, &after);
                    if !self.subscribers.is_empty() {
                        committed = Some(after);
                    }
                }
            }
        }

        let _ = self.x.overlay_hide();
        let _ = self.save();
        let _ = std::fs::remove_file(&socket);
        Ok(())
    }
}

// MARK: pointer and window-move events

impl Daemon {
    fn on_x_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::KeyPress(key) => {
                let state: u16 = key.state.into();
                if std::env::var_os("PERGOLA_DEBUG").is_some() {
                    eprintln!(
                        "pergola: KeyPress detail={} state=0x{state:x} time={} last_release={:?}",
                        key.detail, key.time, self.last_release
                    );
                }
                // Mask to the modifiers a binding can name: Caps Lock and
                // Num Lock are keyboard state, not chords. Matching the raw
                // state is the classic "shortcuts die with numlock on" bug.
                let mods = event_mods(state);
                // A held key auto-repeats: the server sends the same keycode
                // again and again, so a bound chord would fire ~11 times a
                // second (measured on xfwm4+Xvfb: 11 presses in a 1 s hold).
                // Every command here is a discrete action — "back" held for a
                // second would race through eleven history entries — so the
                // first press acts and its repeats are swallowed.
                //
                // A repeat is exactly "a KeyPress whose timestamp equals the
                // preceding KeyRelease's" — the server sends the pair with one
                // timestamp. A deliberate second press always follows a release
                // with a later timestamp, so it fires. The latch is consumed so
                // a stale pair cannot swallow a later press.
                if is_auto_repeat(self.last_release, key.detail, key.time) {
                    self.last_release = None;
                    return Ok(());
                }
                let keysym = self
                    .keymap
                    .iter()
                    .find(|(code, _)| *code == key.detail)
                    .map(|(_, sym)| *sym);
                if let Some(keysym) = keysym {
                    // Owned, because `handle` needs `&mut self` while the
                    // binding table is borrowed.
                    let command = command_for_key(&self.bindings, mods, keysym).map(str::to_string);
                    if let Some(command) = command {
                        let argv: Vec<String> =
                            command.split_whitespace().map(str::to_string).collect();
                        if let Err(e) = self.handle(&argv) {
                            eprintln!("pergola: {command}: {e:#}");
                            // A chord has no terminal, so without this the
                            // failure is invisible. The toast is best effort:
                            // `notify::send` never fails the command.
                            if self.config.notify {
                                crate::notify::send(&format!("{command}: {e:#}"));
                            }
                        }
                    }
                }
            }
            Event::KeyRelease(release) => {
                // Remember when and which key came up. The next KeyPress with
                // the same keycode *and* the same timestamp is the server
                // repeating, not a new press; a real press carries a later
                // timestamp. Modifier state is deliberately not compared: the
                // server sends the release of a grabbed key after the modifiers
                // have gone up when the user lets go of them first (measured
                // under xfwm4: `Ctrl+Alt+Shift+Left` arrives as shift-up
                // state=0xd, ctrl-up 0xc, alt-up 0x8, then Left-up state=0x0).
                if std::env::var_os("PERGOLA_DEBUG").is_some() {
                    eprintln!(
                        "pergola: KeyRelease detail={} state=0x{:x} time={}",
                        release.detail,
                        u16::from(release.state),
                        release.time
                    );
                }
                self.last_release = Some((release.detail, release.time));
            }
            Event::ButtonPress(press) => {
                // Only the primary button drags a boundary, and a press
                // anywhere else means we never saw the previous release.
                if press.detail != 1 || self.band_for_window(press.event).is_none() {
                    self.end_gap_drag()?;
                    return Ok(());
                }
                // The implicit pointer grab keeps the motion stream on this
                // same window until the button is released.
                self.begin_gap_drag(press.event, press.root_x as i32, press.root_y as i32)?;
            }
            Event::MotionNotify(motion) => {
                self.update_gap_drag(motion.root_x as i32, motion.root_y as i32)?;
            }
            Event::ButtonRelease(_) => self.end_gap_drag()?,
            // The gaps carry input-only band windows. Entering one is the only
            // cue that a narrow strip is draggable, so light it up even with
            // the overlay off (UX-NOTES row 1).
            Event::EnterNotify(enter) => self.on_band_enter(enter.event)?,
            Event::LeaveNotify(leave) => self.on_band_leave(leave.event)?,
            Event::ConfigureNotify(configure) => self.note_configure(&configure)?,
            // A pager (taskbar) or an application asking the WM to focus a
            // window: without this a taskbar's "click to focus" is a no-op,
            // which is the largest single failure class in the harvest's
            // ewmh/icccm bucket (168 issues).
            Event::ClientMessage(message) => self.note_client_message(&message)?,
            // A keyboard-map change (layout switch, xmodmap, another client's
            // remap) invalidates both the keycode table and the grabs. Rebuild
            // and re-grab, or every binding silently points at the old layout.
            Event::MappingNotify(_) => {
                for note in self.reinstall_bindings()? {
                    eprintln!("pergola: {note}");
                }
            }
            // A title or class change is a property change, not a configure:
            // without this the tree keeps the name the window had when it was
            // adopted until the next 2 s reconcile — herbstluftwm#35, bspwm#50.
            Event::PropertyNotify(property) => {
                if property.window == self.x.root
                    && self.x.is_client_list_atom(property.atom)
                    && property.state == x11rb::protocol::xproto::Property::NEW_VALUE
                {
                    self.reconcile_layout()?;
                } else if self.x.is_title_atom(property.atom) {
                    if let Ok(Some(info)) = self.x.info(property.window) {
                        if let Some(seat) = self.seats.iter_mut().find(|s| s.window == property.window) {
                            seat.title = info.title;
                            seat.class = info.class;
                        }
                    } else {
                        self.watched.remove(&property.window);
                    }
                }
            }
            // A window we own is gone. Remove its seat now instead of waiting
            // up to 2 s for the timer reconcile: a WM may hand the same XID to
            // a new window inside that window (measured on openbox: a killed
            // xterm's id came back ~0.4 s later), and the lingering seat would
            // make the daemon mistake the new window for the dead one —
            // updating its title and never adopting it.
            Event::DestroyNotify(destroy) => {
                self.watched.remove(&destroy.window);
                if self.seats.iter().any(|s| s.window == destroy.window) {
                    self.reconcile_layout()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Rebuild the keycode table and re-grab every binding after a keyboard
    /// map change. Ungrabbing first drops chords that still point at the old
    /// map; regrabbing the same client is idempotent.
    /// Re-grab every binding and return the notes worth telling the user:
    /// commands with no free key, and commands that fell back to a secondary
    /// chord because the desktop owns the primary one. `reload` returns these
    /// to the caller so a conflict is visible where the command ran, not only
    /// in the journal.
    fn reinstall_bindings(&mut self) -> Result<Vec<String>> {
        if !self.config.hotkeys {
            // Hotkeys were switched off by a reload: drop the grabs so the
            // desktop gets its chords back instead of the daemon holding them.
            let _ = self.x.ungrab_all_keys();
            self.bindings.clear();
            return Ok(vec!["hotkeys are off: no chords are grabbed".to_string()]);
        }
        self.keymap = build_keymap(&self.x)?;
        let _ = self.x.ungrab_all_keys();
        let plan = self.config.binding_plan();
        let mut notes = Vec::new();
        match self.x.install_bindings(&plan) {
            Ok((bound, failures)) => {
                self.bindings = bound;
                for failure in &failures {
                    notes.push(format!(
                        "no free key for '{}' (tried {})",
                        failure.command,
                        failure.tried.join(", ")
                    ));
                }
                for (binding, command) in &self.bindings {
                    if let Some((_, specs)) = plan.iter().find(|(c, _)| c == command) {
                        if let Some(primary) = specs.first() {
                            if binding.label != *primary {
                                notes.push(format!(
                                    "'{}' bound to {} ({} is owned by the desktop)",
                                    command, binding.label, primary
                                ));
                            }
                        }
                    }
                }
            }
            Err(e) => notes.push(format!("cannot grab hotkeys: {e}")),
        }
        Ok(notes)
    }

    fn band_for_window(&self, window: u32) -> Option<&GapHandle> {
        self.gap_bands
            .iter()
            .find(|(id, _)| *id == window)
            .map(|(_, handle)| handle)
    }

    /// Highlight the gap band under the pointer. This is a hint, not the
    /// overlay: it is a separate layer, so it neither claims the overlay is on
    /// nor disappears when the user runs `overlay off`.
    fn on_band_enter(&mut self, window: u32) -> Result<()> {
        if std::env::var_os("PERGOLA_DEBUG").is_some() {
            eprintln!(
                "pergola: enter 0x{window:x} band={} hint={}",
                self.band_for_window(window).is_some(),
                match self.hint {
                    Hint::None => "none",
                    Hint::Hover(_) => "hover",
                    Hint::Drop => "drop",
                }
            );
        }
        if self.hint == Hint::Hover(window) {
            return Ok(());
        }
        // A gap drag owns the pointer, and the drop frame is the useful cue
        // while a window is being dragged — not whichever band the pointer
        // happens to cross. Neither can happen while the WM holds the pointer
        // grab, but the state is checked so a stale watch cannot suppress the
        // hover for good.
        if self.hint == Hint::Drop || self.drag.is_some() {
            return Ok(());
        }
        // Without SHAPE a hint over the gap would take the pointer from the
        // band and flicker; the band still works, it is just not highlighted.
        if !self.x.shape_available() {
            return Ok(());
        }
        let Some(rect) = self
            .band_for_window(window)
            .map(|handle| hint_line(handle.band))
        else {
            return Ok(());
        };
        self.x.hint_show(&[(rect, HINT_COLOR)])?;
        self.hint = Hint::Hover(window);
        Ok(())
    }

    fn on_band_leave(&mut self, window: u32) -> Result<()> {
        if self.hint == Hint::Hover(window) {
            self.hide_hint()?;
        }
        Ok(())
    }

    fn hide_hint(&mut self) -> Result<()> {
        self.drop_edge = None;
        // Unconditional: `hint_show` can fail after creating some windows and
        // before `self.hint` is set, and a guarded call would leave those on
        // screen until the connection closed. `hint_hide` is a no-op when the
        // list is empty.
        self.x.hint_hide()?;
        self.hint = Hint::None;
        Ok(())
    }

    /// Take down a drop frame without touching a hover hint. `settle_moves`
    /// runs on every loop pass, so a blanket `hide_hint` there would erase the
    /// hover line ~50 ms after the pointer entered a gap.
    fn hide_drop_hint(&mut self) -> Result<()> {
        if self.hint == Hint::Drop {
            self.hide_hint()?;
        }
        Ok(())
    }

    /// While a managed window is being dragged, draw the rectangle it will
    /// occupy if it is dropped against the edge the pointer is nearest. The
    /// frame comes from the same `inserting` call the dock itself uses, so it
    /// is the real target, not a half-screen guess — a nested tree docks
    /// against the last focused space, and the hint shows that.
    fn update_drop_hint(&mut self, window: u32) -> Result<()> {
        if self.drag.is_some() || !self.config.edge_dock {
            return Ok(());
        }
        let dragging = self.moves.get(&window).is_some_and(is_drag);
        if !dragging {
            if self.hint == Hint::Drop {
                self.hide_hint()?;
            }
            return Ok(());
        }
        let screen = self.x.workarea_rect()?.to_rect();
        let (px, py) = self.x.pointer()?;
        let edge = pointer_edge(px as f64, py as f64, screen, DROP_EDGE_PX);
        let Some(edge) = edge else {
            if self.hint == Hint::Drop {
                self.hide_hint()?;
            }
            return Ok(());
        };
        if self.hint == Hint::Drop && self.drop_edge == Some(edge) {
            return Ok(());
        }
        if !self.x.shape_available() {
            return Ok(());
        }
        let Some(rect) = self.preview_dock(window, edge) else {
            if std::env::var_os("PERGOLA_DEBUG").is_some() {
                eprintln!("pergola: drop hint: no preview for 0x{window:x} {edge:?}");
            }
            return Ok(());
        };
        if std::env::var_os("PERGOLA_DEBUG").is_some() {
            eprintln!("pergola: drop hint 0x{window:x} {edge:?} -> {rect:?}");
        }
        self.x.hint_show(&frame_bars(rect))?;
        self.hint = Hint::Drop;
        self.drop_edge = Some(edge);
        Ok(())
    }

    /// The screen rectangle the tree would give a window docked against
    /// `edge`, following the same three steps as `dock_window`: choose the
    /// target, take the seat out of the tree, then insert it against the edge.
    /// A preview that skipped the detach would draw the frame from a tree that
    /// still holds the window, so the frame would not be where the dock lands.
    fn preview_dock(&self, window: u32, edge: Direction) -> Option<Rect> {
        let screen = self.x.workarea_rect().ok()?.to_rect();
        let world = screen;
        let seat = self.seat_for_window(window)?;
        // Selection order copied from `dock_window` verbatim: a preview that
        // chose a different target would draw its frame where the window is
        // not going to land.
        let mut target = self
            .focused_panel()
            .unwrap_or_else(|| self.file.doc.root.id().to_string());
        if target == seat.panel {
            target = self
                .last_focus
                .clone()
                .filter(|p| p != &seat.panel && self.file.doc.root.node(p).is_some())
                .unwrap_or_else(|| self.file.doc.root.id().to_string());
        }
        // A seat that is already floating is not in the tree, so `removing_view`
        // fails and the tree stands as it is — the same branch `detach_seat`
        // takes.
        let detached = self
            .engine
            .removing_view(&seat.seat, &self.file.doc.root)
            .unwrap_or_else(|_| self.file.doc.root.clone());
        if detached.node(&target).is_none() {
            target = detached.id().to_string();
        }
        let probe = self.free_panel_id(&seat);
        let node = LayoutNode::panel(probe.clone(), vec![seat.seat.clone()]);
        let tree = self.engine.inserting(node, &target, edge, &detached).ok()?;
        let rects = self.engine.rects(&tree, world);
        let rect = rects.iter().find(|(id, _)| id == &probe).map(|(_, r)| *r)?;
        let framed = if self.camera.framed != self.file.doc.root.id() {
            rects
                .iter()
                .find(|(id, _)| id == &self.camera.framed)
                .map(|(_, r)| *r)
        } else {
            None
        };
        Some(transform(rect, framed, world))
    }

    /// Start dragging a split boundary. The tree is snapshotted so that every
    /// motion is absolute, not incremental.
    fn begin_gap_drag(&mut self, window: u32, x: i32, y: i32) -> Result<()> {
        let (first, second, axis, extent) = match self.band_for_window(window) {
            Some(handle) => (
                handle.first.clone(),
                handle.second.clone(),
                handle.axis,
                handle.extent.max(1.0),
            ),
            None => return Ok(()),
        };
        // A missed release leaves the previous gesture open; commit it before
        // starting another so its in-flight layout is not silently dropped.
        if self.drag.is_some() {
            self.end_gap_drag()?;
        }
        // The hover hint belongs to the band about to be replaced by this
        // gesture; `end_gap_drag` rebuilds the bands, so the hovered window
        // would never send the LeaveNotify that takes the hint down.
        self.hide_hint()?;
        if std::env::var_os("PERGOLA_DEBUG").is_some() {
            eprintln!(
                "pergola: gap drag {first}|{second} axis {axis:?} extent {extent:.0}px"
            );
        }
        // A gap drag is a layout change like any other: remember the tree the
        // user started from so `undo` puts the split back.
        self.undo.push(self.snapshot());
        self.drag = Some(GapDrag {
            first,
            second,
            axis,
            extent,
            start: (x, y),
            snapshot: self.file.doc.root.clone(),
        });
        Ok(())
    }

    /// Move the dragged boundary: the pointer delta becomes a weight delta
    /// against the split's displayed length.
    fn update_gap_drag(&mut self, x: i32, y: i32) -> Result<()> {
        let Some(drag) = &self.drag else {
            return Ok(());
        };
        let delta_px = match drag.axis {
            Axis::X => (x - drag.start.0) as f64,
            Axis::Y => (y - drag.start.1) as f64,
        };
        let delta = delta_px / drag.extent;
        let snapshot = drag.snapshot.clone();
        let (first, second) = (drag.first.clone(), drag.second.clone());
        match self.engine.resizing_gap(&first, &second, delta, &snapshot) {
            Ok(tree) => {
                self.replace_root(tree);
                self.normalize_camera();
                self.apply()?;
            }
            Err(e) => {
                // Clamped at the minimum share: the layout is already as far as
                // it may go, so this is not an error the user needs to see.
                if std::env::var_os("PERGOLA_DEBUG").is_some() {
                    eprintln!("pergola: gap drag: {e}");
                }
            }
        }
        Ok(())
    }

    fn end_gap_drag(&mut self) -> Result<()> {
        if self.drag.take().is_none() {
            return Ok(());
        }
        // Rebuild the bands before saving: a failed write must not leave the
        // pointer without grab areas until the next apply.
        let bands = self.refresh_gap_bands();
        let saved = self.save();
        bands?;
        saved
    }

    /// Track a configure event that may be the user dragging a window. Our own
    /// moves are ignored (they match what we asked for, and echo right after an
    /// apply); a drag is a burst of changes followed by silence.
    fn note_configure(&mut self, event: &ConfigureNotifyEvent) -> Result<()> {
        if event.override_redirect || self.drag.is_some() {
            return Ok(());
        }
        // Our own apply echoes back as configure events; a window after it is
        // not a user drag. (Removing this and relying only on the applied rect
        // was measured 2026-11-15: intermediate echoes of a two-step apply read
        // as drags and every window docked itself. The openbox cost of keeping
        // it is filed as pi-agenda bug 'edge drag under openbox'.)
        //
        // 100 ms, not 250 ms: measured 2026-11-15, the stale ConfigureNotify a
        // WM delivers after our move arrives within ~0.5 ms of the apply (a
        // pre-move echo the WM had already queued). 100 ms still catches that
        // burst while shrinking the window in which a real drag can be
        // swallowed 2.5x; both WMs passed 43/43 three runs each at 100 ms.
        //
        // An event inside the guard is not discarded: the newest rect is kept
        // and re-tested after the guard, by the window's *live* geometry (see
        // `settle_deferred`). Removing the guard outright was measured
        // 2026-11-15: intermediate echoes of a two-step apply read as drags and
        // every window docked itself (openbox e2e 1 -> 7 failures).
        if self.seat_for_window(event.window).is_none() {
            return Ok(());
        }
        // A fullscreen window's geometry is the WM's (F11), not a drag the
        // user wants docked; and a transient window's place belongs to its
        // parent. Treating either as a drag docks the wrong window to the
        // nearest edge — awesome#22, i3#594.
        if let Ok(Some(info)) = self.x.info(event.window) {
            if info.fullscreen || info.transient {
                return Ok(());
            }
        }
        // The WM reports client coordinates; edge snapping is about the frame
        // the user sees, so add the decorations back.
        let (l, r, t, b) = self.x.frame_extents(event.window);
        let rect = Rect::new(
            (i32::from(event.x) - l) as f64,
            (i32::from(event.y) - t) as f64,
            (i32::from(event.width) + l + r) as f64,
            (i32::from(event.height) + t + b) as f64,
        );
        if std::env::var_os("PERGOLA_DEBUG").is_some() {
            eprintln!(
                "pergola: configure 0x{:x} rect={:?} applied={:?} seat={}",
                event.window,
                rect,
                self.applied.get(&event.window).map(|r| r.to_rect()),
                self.seat_for_window(event.window).is_some(),
            );
        }
        if self.last_apply.elapsed() < APPLY_GUARD {
            self.deferred.insert(
                event.window,
                DeferredConfigure {
                    rect,
                    stable: None,
                },
            );
            return Ok(());
        }
        self.track_configure(event.window, rect)
    }

    /// Record a configure rect for a seat after the apply guard has passed.
    /// The rect is what the window reported (or, for a deferred event, what the
    /// server says it is now), so an echo of our own apply is still skipped.
    fn track_configure(&mut self, window: u32, rect: Rect) -> Result<()> {
        let applied = self.applied.get(&window).map(|r| r.to_rect());
        if let Some(applied) = applied {
            // A window we parked off the work area is not being dragged: its
            // title bar is not on screen to grab. Some WMs (openbox) pull such
            // a window back so a sliver stays reachable, which reports a rect
            // 44 px from the one we asked for — far enough to read as a drag,
            // after which the parked tile re-docks itself into the tree. That
            // is the churn behind the openbox e2e's varying failures (measured
            // 2026-11-23: 'holding a chord goes back once' and 'undo restored a
            // third tree' both came from a parked tile re-docking during an
            // unrelated step). The clamp is the WM's placement, not a gesture.
            if self
                .last_screen()
                .is_some_and(|screen| parked_off_screen(applied, screen))
            {
                return Ok(());
            }
            // An echo of our own apply reports the rect we asked for. A drag
            // already in progress is different: the WM is reporting real
            // motion, and a drop that lands exactly on the rect we last placed
            // the window at is still a drop — the user dragged it away and
            // back, or the tile was already against that edge. Skipping it
            // left the watch on the previous intermediate rect, so a corner
            // drop docked against the wrong edge and the pointer's final
            // position never drew its hint (measured 2026-11-15, e2e
            // 'no drop target during a drag': the reported rect was 83 px from
            // the right edge because only the intermediate events counted).
            let drag_in_progress = self.moves.get(&window).is_some_and(is_drag);
            if same_rect_within(applied, rect, DECORATION_SLOP_PX) && !drag_in_progress {
                return Ok(());
            }
        }
        // The baseline is where we last put the window ourselves. A WM that
        // reports the drop position only (openbox sends one ICCCM-synthetic
        // ConfigureNotify at the end of a drag, xfwm4 sends a stream) still
        // shows a real travel from that baseline; a window the WM merely
        // clamped, or our own echo, does not move. Measured 2026-11-15: with a
        // strict "three configure events" rule, edge docking never fired under
        // openbox and the e2e suite's one failing assertion was this path.
        let baseline = applied;
        let moved_from_baseline =
            baseline.is_some_and(|b| !same_rect_within(b, rect, DECORATION_SLOP_PX));
        let watch = self.moves.entry(window).or_insert(MoveWatch {
            moves: u32::from(moved_from_baseline),
            at: Instant::now(),
            rect,
            start: baseline.unwrap_or(rect),
        });
        if !same_rect(watch.rect, rect) {
            watch.moves += 1;
            watch.rect = rect;
            watch.at = Instant::now();
        }
        self.update_drop_hint(window)?;
        Ok(())
    }

    /// Re-test configures that arrived inside the own-apply guard. Acting on
    /// them then only sees our own echo (its rect is what we asked for); once
    /// the guard and the window have settled, the live geometry says whether
    /// anything really moved. Openbox's single drop event is the reason this
    /// exists: it arrives right after a re-apply and was previously dropped.
    fn settle_deferred(&mut self) -> Result<()> {
        if self.deferred.is_empty() || self.last_apply.elapsed() < APPLY_GUARD {
            return Ok(());
        }
        let now = Instant::now();
        let windows: Vec<u32> = self.deferred.keys().copied().collect();
        let mut ready = Vec::new();
        for window in windows {
            let Some(entry) = self.deferred.get(&window) else {
                continue;
            };
            let remembered = entry.rect;
            let stable = entry.stable;
            let live = match self.x.info(window) {
                Ok(Some(info)) if !info.fullscreen && !info.transient => frame_rect(&info),
                _ => {
                    self.deferred.remove(&window);
                    continue;
                }
            };
            let applied = self.applied.get(&window).map(|r| r.to_rect());
            match deferred_action(applied, remembered, live, stable, now) {
                DeferredAction::Discard => {
                    self.deferred.remove(&window);
                }
                DeferredAction::Restart(rect) => {
                    if let Some(entry) = self.deferred.get_mut(&window) {
                        entry.rect = rect;
                        entry.stable = None;
                    }
                }
                DeferredAction::Wait => {
                    if let Some(entry) = self.deferred.get_mut(&window) {
                        entry.stable.get_or_insert(now);
                    }
                }
                DeferredAction::Ready(rect) => {
                    self.deferred.remove(&window);
                    ready.push((window, rect));
                }
            }
        }
        for (window, rect) in ready {
            self.track_configure(window, rect)?;
        }
        Ok(())
    }

    /// A window that stopped moving after a real drag and sits against a screen
    /// edge is docked to that edge. This is the mouse path for tiling: drag a
    /// window to the left, right, top or bottom of the work area and let go.
    fn settle_moves(&mut self) -> Result<()> {
        if !self.config.edge_dock {
            self.moves.clear();
            self.deferred.clear();
            self.hide_drop_hint()?;
            return Ok(());
        }
        if self.drag.is_some() {
            return Ok(());
        }
        self.settle_deferred()?;
        let now = Instant::now();
        let mut ready = Vec::new();
        let mut stale = Vec::new();
        for (window, watch) in &self.moves {
            if std::env::var_os("PERGOLA_DEBUG").is_some() {
                eprintln!(
                    "pergola: move-watch 0x{:x} moves={} rect={:?} start={:?} age={:?} drag={}",
                    window,
                    watch.moves,
                    watch.rect,
                    watch.start,
                    now.duration_since(watch.at),
                    is_drag(watch),
                );
            }
            if now.duration_since(watch.at) >= Duration::from_millis(300) {
                if is_drag(watch) {
                    ready.push((*window, watch.rect));
                } else {
                    // A one-pixel WM echo must not wait indefinitely for a
                    // future layout change to turn it into a false drag.
                    stale.push(*window);
                }
            }
        }
        for window in stale {
            self.moves.remove(&window);
        }
        if ready.is_empty() {
            // No watch counts as a drag any more: a frame still up belongs to
            // a gesture that ended without a dockable drop. Only the drop
            // frame: a hover hint is not this function's business.
            if !self.moves.values().any(|w| is_drag(w)) {
                self.hide_drop_hint()?;
            }
            return Ok(());
        }
        // The gesture is over: the drop frame has done its job.
        self.hide_drop_hint()?;
        let screen = self.x.workarea_rect()?.to_rect();
        let world = screen;
        let desk = self
            .engine
            .rect_of(&self.file.doc.root, world, "desktop")
            .unwrap_or(world);
        // The drop rectangle is in screen space; a zoomed view must be undone
        // before it can be stored as a world position.
        let rects = self.engine.rects(&self.file.doc.root, world);
        let framed = if self.camera.framed != self.file.doc.root.id() {
            rects
                .iter()
                .find(|(id, _)| id == &self.camera.framed)
                .map(|(_, r)| *r)
        } else {
            None
        };
        let mut marked = false;
        let mut pushed: Option<Snapshot> = None;
        for (window, rect) in ready {
            self.moves.remove(&window);
            let Some(seat) = self.seat_for_window(window).cloned() else {
                continue;
            };
            // A watch records where the gesture last put the window. If the
            // window has moved since (a layout apply overtook a pending drag),
            // docking or floating it from the recorded rect would move it
            // somewhere the user never dropped it — and the undo entry that
            // gesture pushes would then restore the layout already on screen
            // (measured 2026-11-19, #231: a chord `dock left` overtook a 0.3 s
            // `wmctrl` move, the stale watch re-floated the tile, and `undo`
            // put back the post-dock tree).
            let live = match self.x.info(window) {
                Ok(Some(info)) if !info.fullscreen && !info.transient => Some(frame_rect(&info)),
                _ => None,
            };
            if live.is_some_and(|l| watch_overtaken(rect, l)) {
                if std::env::var_os("PERGOLA_DEBUG").is_some() {
                    eprintln!(
                        "pergola: stale watch 0x{window:x} recorded={rect:?} live={:?}, dropped",
                        live.map(|l| l.to_irect())
                    );
                }
                continue;
            }
            if !marked {
                // One snapshot per gesture, taken before the first window is
                // actually docked or floated.
                let before = self.snapshot();
                self.undo.push(before.clone());
                pushed = Some(before);
                marked = true;
            }
            if self.file.doc.root.node(&seat.panel).is_some() {
                // A tile dragged onto a different edge re-docks against it in
                // one gesture, instead of the two drags float-then-dock took
                // (UX-NOTES row 7). Dropping it back on its own edge keeps the
                // old float behaviour, so "pull it off the edge" still works.
                let here = rects
                    .iter()
                    .find(|(id, _)| id == &seat.panel)
                    .map(|(_, r)| transform(*r, framed, world));
                // A tile that merely spans the edge it was dropped on (a
                // full-width row touches left and right alike) must not look
                // like "it is already there": what counts is whether the drag
                // took the window out of its own rectangle at all.
                let moved_out = here.is_none_or(|h| {
                    !(rect.mid_x() >= h.x
                        && rect.mid_x() <= h.max_x()
                        && rect.mid_y() >= h.y
                        && rect.mid_y() <= h.max_y())
                });
                if let Some(edge) = snap_edge(rect, screen, EDGE_SNAP_PX) {
                    if moved_out {
                        if std::env::var_os("PERGOLA_DEBUG").is_some() {
                            eprintln!("pergola: re-dock 0x{window:x} -> {edge:?}");
                        }
                        // `dock_window` takes the seat out of the tree and
                        // inserts it against the edge in one step. Floating in
                        // between would save a layout the user never asked for
                        // and, if the dock then failed, leave the tile floating.
                        if let Err(e) = self.dock_window(window, edge, None) {
                            eprintln!("pergola: re-dock: {e:#}");
                        }
                        continue;
                    }
                }
                // A tile pulled off the tree: float it where it was dropped.
                if std::env::var_os("PERGOLA_DEBUG").is_some() {
                    eprintln!("pergola: drag out 0x{window:x} -> floating");
                }
                let dropped = into_world(untransform(rect, framed, world), Some(desk), world);
                if let Err(e) = self.float_window(window, Some(dropped)) {
                    eprintln!("pergola: drag out: {e:#}");
                }
                continue;
            }
            let Some(edge) = snap_edge(rect, screen, EDGE_SNAP_PX) else {
                continue;
            };
            if std::env::var_os("PERGOLA_DEBUG").is_some() {
                eprintln!("pergola: edge drop 0x{window:x} -> {edge:?}");
            }
            if let Err(e) = self.dock_window(window, edge, None) {
                eprintln!("pergola: edge dock: {e:#}");
            }
        }
        // An undo entry that restores the state already on screen is not an
        // undo: the next `undo` would appear to do nothing. A gesture that was
        // a no-op (a window already against its edge) must leave none.
        if pushed.is_some_and(|s| s == self.snapshot()) {
            let _ = self.undo.pop();
        }
        Ok(())
    }
}

/// The records an event-stream client is sent when the committed state
/// changes. Pure, so the transition table is unit-tested without X: a `dock`
/// is a window that moved into the tree, `undock` is one that left it for the
/// desktop, `window-opened`/`window-closed` are appearance and disappearance,
/// `layout` is any other change to the tree, and `focus` is a change of the
/// active window.
fn diff_events(before: &EventSig, after: &EventSig) -> Vec<serde_json::Value> {
    use serde_json::json;
    let mut out = Vec::new();
    for (window, (placement, title)) in &after.placements {
        match before.placements.get(window) {
            None => {
                let (kind, panel) = match placement {
                    Placement::Docked(p) => ("dock", p),
                    Placement::Floating(p) => ("window-opened", p),
                };
                out.push(json!({"type": kind, "window": window, "title": title, "panel": panel}));
            }
            Some((old, _)) if old != placement => {
                let (kind, panel) = match placement {
                    Placement::Docked(p) => ("dock", p),
                    Placement::Floating(p) => ("undock", p),
                };
                out.push(json!({"type": kind, "window": window, "title": title, "panel": panel}));
            }
            Some(_) => {}
        }
    }
    for (window, (_, title)) in &before.placements {
        if !after.placements.contains_key(window) {
            out.push(json!({"type": "window-closed", "window": window, "title": title}));
        }
    }
    if before.root != after.root {
        out.push(json!({
            "type": "layout",
            "tree": serde_json::from_str::<serde_json::Value>(&after.root)
                .unwrap_or(serde_json::Value::Null),
        }));
    }
    if before.active != after.active {
        let title = after
            .placements
            .get(&after.active)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        out.push(json!({"type": "focus", "window": after.active, "title": title}));
    }
    if before.tag != after.tag {
        out.push(json!({"type": "tag", "name": after.tag}));
    }
    out
}

/// Which seat an EWMH `_NET_ACTIVE_WINDOW` request should focus: the seat id,
/// the panel to talk about, and whether that panel holds the seat as a hidden
/// tab view that must be shown first. `None` means the request names a window
/// we do not manage (a stale pager entry, an override-redirect popup), which
/// must be ignored rather than focused off the tree — xmonad's `EwmhDesktops`
/// drops unmanaged windows for the same reason.
fn activation_target(
    seats: &[SeatRecord],
    root: &LayoutNode,
    window: u32,
) -> Option<(String, String, bool)> {
    let seat = seats.iter().find(|s| s.window == window)?;
    match root.panel_containing(&seat.seat) {
        Some(panel) => {
            let hidden =
                panel.is_tab_group() && panel.selected.as_deref() != Some(seat.seat.as_str());
            Some((seat.seat.clone(), panel.id.clone(), hidden))
        }
        // A floating window is not in the tree: there is no view to switch, it
        // only has to be focused. Its seat record still names the panel it owns.
        None => Some((seat.seat.clone(), seat.panel.clone(), false)),
    }
}

/// Whether `window` is one of our own recent activation requests echoing back.
/// A match is removed, so one request cannot be consumed twice; a request older
/// than [`ACTIVATION_ECHO`] is somebody else's and must be handled.
fn take_own_activation(tracked: &mut Vec<(u32, Instant)>, window: u32, now: Instant) -> bool {
    let Some(idx) = tracked
        .iter()
        .position(|(w, at)| *w == window && now.duration_since(*at) < ACTIVATION_ECHO)
    else {
        return false;
    };
    tracked.remove(idx);
    true
}

/// How long our own `_NET_ACTIVE_WINDOW` request can take to echo back through
/// the root selection before a matching request is treated as somebody else's.
/// Generous on purpose: a loaded daemon can take a second to drain its event
/// queue, and a missed echo costs one redundant activation round. A false match
/// costs only a swallowed request for a window we just focused ourselves.
const ACTIVATION_ECHO: Duration = Duration::from_secs(2);

/// How many of our own requests can be waiting for an echo at once.
const ACTIVATION_ECHO_DEPTH: usize = 8;

/// Colour of a pointer hint: the accent the overlay uses for the framed
/// split, so a hint reads as "pergola" and not as a client window.
const HINT_COLOR: u32 = 0x00e8b04b;

/// How close the pointer must be to a screen edge for a dragged window to show
/// a drop target. Larger than the docking slack: the frame appears while the
/// user is still aiming, the dock itself happens on release.
const DROP_EDGE_PX: f64 = 70.0;

/// Event-stream limits. A bar that stops reading is dropped after this long
/// without draining anything, and never holds more than this many records in
/// its queue — i3's bounded per-client queue and 10 s `kill_timeout`.
const SUB_TIMEOUT: Duration = Duration::from_secs(10);
const SUB_QUEUE_MAX: usize = 256;
const SUB_MAX_CLIENTS: usize = 16;

/// A gap band as a 3 px line down its narrow axis. The band itself is a strip
/// the width of the gap, too faint to see as a highlight.
fn hint_line(band: Rect) -> IRect {
    const W: f64 = 3.0;
    let (x, y, w, h) = if band.w <= band.h {
        (band.x + (band.w - W) / 2.0, band.y, W, band.h)
    } else {
        (band.x, band.y + (band.h - W) / 2.0, band.w, W)
    };
    IRect::new(
        x.floor() as i32,
        y.floor() as i32,
        w.round().max(1.0) as i32,
        h.round().max(1.0) as i32,
    )
}

/// A drop target as its four edges: a frame over the content instead of a
/// filled rectangle that would hide the windows underneath it.
fn frame_bars(rect: Rect) -> Vec<(IRect, u32)> {
    const W: f64 = 3.0;
    let (x, y) = (rect.x.round() as i32, rect.y.round() as i32);
    let (w, h) = (rect.w.round().max(1.0) as i32, rect.h.round().max(1.0) as i32);
    let vertical = |left: i32| IRect::new(left, y, W as i32, h);
    let horizontal = |top: i32| IRect::new(x, top, w, W as i32);
    vec![
        (vertical(x), HINT_COLOR),
        (vertical(x + w - W as i32), HINT_COLOR),
        (horizontal(y), HINT_COLOR),
        (horizontal(y + h - W as i32), HINT_COLOR),
    ]
}

/// The edge of `screen` the pointer is nearest, if it is within `slack`.
fn pointer_edge(x: f64, y: f64, screen: Rect, slack: f64) -> Option<Direction> {
    let candidates = [
        (x - screen.x, Direction::Left),
        (screen.max_x() - x, Direction::Right),
        (y - screen.y, Direction::Up),
        (screen.max_y() - y, Direction::Down),
    ];
    candidates
        .into_iter()
        .filter(|(distance, _)| *distance <= slack)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, direction)| direction)
}

/// How long after our own apply a ConfigureNotify is treated as its echo.
const APPLY_GUARD: Duration = Duration::from_millis(100);

/// How far an echo of our own apply may differ from the rect we asked for and
/// still be recognised as ours. A window manager adds the decoration back to
/// the client rect it reports, and the two do not have to agree with ours about
/// which rect the request named: openbox reports the client at exactly the
/// position the daemon asked for, so the frame rect we compute from it is one
/// pixel left and one pixel up from the rect we recorded (measured 2026-11-23).
/// At 0.5 px that echo read as somebody else's move, and every apply grew a
/// phantom movement watch.
const DECORATION_SLOP_PX: f64 = 2.0;

/// How long a deferred event's live rect must stand still before it is judged.
const DEFERRED_STABLE: Duration = Duration::from_millis(80);

/// The frame rectangle of a live window: client origin minus the decorations.
fn frame_rect(info: &WindowInfo) -> Rect {
    let (l, r, t, b) = info.extents;
    Rect::new(
        (info.rect.x - l) as f64,
        (info.rect.y - t) as f64,
        (info.rect.w + l + r) as f64,
        (info.rect.h + t + b) as f64,
    )
}

/// A settled window counts as dragged when it produced a burst of configure
/// events, or when one event moved it far enough that no WM placement could
/// explain it (a single-report WM: openbox).
fn is_drag(watch: &MoveWatch) -> bool {
    if watch.moves >= 3 {
        return true;
    }
    if watch.moves == 0 {
        return false;
    }
    let travel = (watch.rect.x - watch.start.x)
        .abs()
        .max((watch.rect.y - watch.start.y).abs());
    travel >= DRAG_TRAVEL_PX
}

/// A movement watch is a gesture only while the window is still where the
/// gesture left it. If the window has moved since (a layout apply, the WM
/// re-placing it), the recorded rect is history: docking or floating from it
/// would move a window the user never dropped there.
///
/// Rounding and decoration echoes move a rect by a pixel or two (openbox
/// reports `y: 452` for a window the daemon placed at 453), so "no longer
/// there" is a move larger than an edge snap — not an exact comparison.
fn watch_overtaken(recorded: Rect, live: Rect) -> bool {
    let travel = (recorded.x - live.x)
        .abs()
        .max((recorded.y - live.y).abs())
        .max((recorded.w - live.w).abs())
        .max((recorded.h - live.h).abs());
    travel > EDGE_SNAP_PX
}

/// How far a single-event move must travel to be a drag rather than placement.
const DRAG_TRAVEL_PX: f64 = 40.0;

/// How close to a screen edge a dragged window must be dropped to tile.
const EDGE_SNAP_PX: f64 = 14.0;

/// How many layout changes `pergola undo` can walk back.
const UNDO_DEPTH: usize = 10;

/// How dark the overlay's dim layer is, in the X opacity convention: a 32-bit
/// CARDINAL where `0xffffffff` is opaque. 40% black leaves the tiles readable
/// while making the 2 px split lines read at a glance.
/// The dim layer's strength is `config.dim`; this is only the percentage the
/// command reply and the e2e talk about.
fn dim_percent(dim: f64) -> u32 {
    (dim.clamp(0.0, 0.9) * 100.0).round() as u32
}

/// The screen edge a rectangle is sitting against, if any. Corners go to the
/// nearer edge; a window in the middle returns `None`.
fn snap_edge(rect: Rect, screen: Rect, slack: f64) -> Option<Direction> {
    let candidates = [
        (rect.x - screen.x, Direction::Left),
        (screen.max_x() - rect.max_x(), Direction::Right),
        (rect.y - screen.y, Direction::Up),
        (screen.max_y() - rect.max_y(), Direction::Down),
    ];
    candidates
        .into_iter()
        .filter(|(distance, _)| *distance <= slack)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, direction)| direction)
}

fn parse_command(name: &str, rest: &[String]) -> Result<Command> {
    match name {
        "neighbour" => {
            let dir = rest
                .first()
                .and_then(|s| Direction::parse(s))
                .ok_or_else(|| anyhow!("usage: pergola neighbour <left|right|up|down>"))?;
            Ok(Command::Neighbour(dir))
        }
        "view" | "save-view" => {
            let n: u8 = rest
                .first()
                .ok_or_else(|| anyhow!("usage: pergola {name} <1-9>"))?
                .parse()
                .map_err(|_| anyhow!("slot must be 1-9"))?;
            if !(1..=9).contains(&n) {
                return Err(anyhow!("slot must be 1-9"));
            }
            Ok(if name == "view" {
                Command::View(n)
            } else {
                Command::SaveView(n)
            })
        }
        other => Command::parse(other).map_err(|e| anyhow!(e)),
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn parse_window(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    if let Some(hex) = raw.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).ok()
    } else {
        raw.parse().ok()
    }
}

/// Camera transform: when a node is framed, scale the world so that node fills
/// the screen. Everything else moves with it, which is what makes the zoom
/// continuous rather than a workspace switch.
fn transform(rect: Rect, framed: Option<Rect>, screen: Rect) -> Rect {
    let Some(f) = framed else { return rect };
    if f.w <= 0.0 || f.h <= 0.0 {
        return rect;
    }
    // Non-uniform: the framed node maps exactly onto the screen, so framing a
    // window behaves like maximising it, and everything else moves with it.
    let sx = screen.w / f.w;
    let sy = screen.h / f.h;
    // The framed node fills the *work area*, so a work area that does not start
    // at 0,0 (a top panel) lands where the window manager expects it.
    Rect::new(
        screen.x + (rect.x - f.x) * sx,
        screen.y + (rect.y - f.y) * sy,
        rect.w * sx,
        rect.h * sy,
    )
}

/// How many screen pixels one world pixel became, along the axis a boundary
/// moves on. The camera transform is non-uniform, so X and Y differ.
fn axis_scale(axis: Axis, world: Rect, shown: Rect) -> f64 {
    match axis {
        Axis::X if world.w > 0.0 => shown.w / world.w,
        Axis::Y if world.h > 0.0 => shown.h / world.h,
        _ => 1.0,
    }
}

/// The inverse of [`transform`]: a screen rectangle back into world space.
fn untransform(rect: Rect, framed: Option<Rect>, screen: Rect) -> Rect {
    let Some(f) = framed else { return rect };
    if f.w <= 0.0 || f.h <= 0.0 || screen.w <= 0.0 || screen.h <= 0.0 {
        return rect;
    }
    let sx = screen.w / f.w;
    let sy = screen.h / f.h;
    Rect::new(
        f.x + (rect.x - screen.x) / sx,
        f.y + (rect.y - screen.y) / sy,
        rect.w / sx,
        rect.h / sy,
    )
}

/// Ids on the path from the root to `target`, inclusive.
fn ancestors<'a>(root: &'a LayoutNode, target: &str) -> Vec<&'a str> {
    fn walk<'a>(node: &'a LayoutNode, target: &str, acc: &mut Vec<&'a str>) -> bool {
        acc.push(node.id());
        if node.id() == target {
            return true;
        }
        for child in node.children() {
            if walk(child, target, acc) {
                return true;
            }
        }
        acc.pop();
        false
    }
    let mut acc = Vec::new();
    if walk(root, target, &mut acc) {
        acc
    } else {
        Vec::new()
    }
}

/// Bars that visualise the tree: one 2 px line per split boundary, centred in
/// the gap, plus a 3 px frame around the viewport when a node is framed.
///
/// Lines live in the gaps, so the overlay never covers a window's title bar or
/// border — the previous design drew a border around every tile, which read as
/// a debug artefact on top of the decorations.
fn overlay_bars(
    root: &LayoutNode,
    rects: &[(String, Rect)],
    framed: Option<&str>,
    gap: f64,
    viewport: Rect,
    wide: bool,
) -> Vec<(IRect, u32)> {
    const LINE: u32 = 0x00505f6b;
    const ACCENT: u32 = 0x00e8b04b;
    // 4 px with no dim layer (the line has to carry the cue on its own), 2 px
    // when a compositor dims the tiles behind it.
    let t: i32 = if wide { 4 } else { 2 };
    let path = framed.map(|f| ancestors(root, f)).unwrap_or_default();
    let mut bars = Vec::new();
    for node in root.flatten() {
        let LayoutNode::Split(split) = node else { continue };
        let Some((_, rect)) = rects.iter().find(|(id, _)| id == &split.id) else {
            continue;
        };
        let accent = framed.is_some() && path.contains(&split.id.as_str());
        let color = if accent { ACCENT } else { LINE };
        for i in 0..split.children.len().saturating_sub(1) {
            // A zero-weight child (a collapsed desktop leaf) has no boundary to
            // draw: the line would sit on the screen edge for nothing.
            if split.weights.get(i).copied().unwrap_or(0.0) <= 0.0
                || split.weights.get(i + 1).copied().unwrap_or(0.0) <= 0.0
            {
                continue;
            }
            let Some((_, child)) = rects.iter().find(|(id, _)| id == split.children[i].id()) else {
                continue;
            };
            match split.axis {
                Axis::X => {
                    let x = child.max_x() + gap / 2.0 - t as f64 / 2.0;
                    bars.push((
                        IRect::new(x.round() as i32, rect.y.round() as i32, t, rect.h.round() as i32),
                        color,
                    ));
                }
                Axis::Y => {
                    let y = child.max_y() + gap / 2.0 - t as f64 / 2.0;
                    bars.push((
                        IRect::new(rect.x.round() as i32, y.round() as i32, rect.w.round() as i32, t),
                        color,
                    ));
                }
            }
        }
    }
    if framed.is_some() {
        let t = 3;
        let v = viewport.to_irect();
        bars.push((IRect::new(v.x, v.y, v.w, t), ACCENT));
        bars.push((IRect::new(v.x, v.y + v.h - t, v.w, t), ACCENT));
        bars.push((IRect::new(v.x, v.y, t, v.h), ACCENT));
        bars.push((IRect::new(v.x + v.w - t, v.y, t, v.h), ACCENT));
    }
    bars
}

/// A window that is entirely outside the framed view is parked *below* the work
/// area. Parking to the side does not work: xfwm4 (and most WMs) clamp a window
/// so a strip of its title bar stays reachable, which leaves a visible sliver at
/// the screen edge. Below the work area is not clamped.
fn park_if_outside(target: IRect, viewport: IRect, zoomed: bool) -> IRect {
    if !zoomed {
        return target;
    }
    let visible = target.x < viewport.x + viewport.w
        && target.x + target.w > viewport.x
        && target.y < viewport.y + viewport.h
        && target.y + target.h > viewport.y;
    if visible {
        target
    } else {
        IRect::new(viewport.x, viewport.y + viewport.h, target.w, target.h)
    }
}

/// Is `window` on a host desktop other than the one the WM is showing?
///
/// Pergola tiles *inside* one desktop. The host WM's workspaces are not
/// pergola's tags: a window the user moved to another desktop belongs to the WM
/// there, and moving it from here drags it back onto this one — the measured
/// `pi-tile --park` failure. `Some(0xFFFFFFFF)` is the EWMH "on every desktop"
/// value (a sticky window), and a missing property means "cannot tell", so
/// neither is foreign.
fn on_another_desktop(current: Option<u32>, window: Option<u32>) -> bool {
    match (current, window) {
        (Some(current), Some(window)) => window != current && window != u32::MAX,
        _ => false,
    }
}

fn same_rect(a: Rect, b: Rect) -> bool {
    same_rect_within(a, b, 0.5)
}

/// The same comparison with a stated tolerance, for the places where a window
/// manager's decoration rounding is expected (see `DECORATION_SLOP_PX`) and an
/// exact comparison would call our own placement somebody else's move.
fn same_rect_within(a: Rect, b: Rect, slop: f64) -> bool {
    (a.x - b.x).abs() <= slop
        && (a.y - b.y).abs() <= slop
        && (a.w - b.w).abs() <= slop
        && (a.h - b.h).abs() <= slop
}

/// Whether two rectangles share any area. A rect with a zero side (a collapsed
/// desktop leaf) shares nothing.
fn overlaps(a: Rect, b: Rect) -> bool {
    a.w > 0.0
        && a.h > 0.0
        && b.w > 0.0
        && b.h > 0.0
        && a.x < b.max_x()
        && a.max_x() > b.x
        && a.y < b.max_y()
        && a.max_y() > b.y
}

/// True when the rect we last *placed* a window at has left the work area — the
/// daemon parks a window below the screen to take it off screen, so a configure
/// for such a window is the window manager's placement, never a drag.
fn parked_off_screen(applied: Rect, screen: Rect) -> bool {
    !overlaps(applied, screen)
}

/// Map a world rect (0..screen) into the rectangle the desktop leaf occupies.
fn into_screen(rect: Rect, desk: Option<Rect>, world: Rect) -> Rect {
    let Some(d) = desk else { return rect };
    if world.w <= 0.0 || world.h <= 0.0 || d.w <= 0.0 || d.h <= 0.0 {
        return rect;
    }
    if (d.x - world.x).abs() < 1.0
        && (d.y - world.y).abs() < 1.0
        && (d.w - world.w).abs() < 1.0
        && (d.h - world.h).abs() < 1.0
    {
        return rect;
    }
    let sx = d.w / world.w;
    let sy = d.h / world.h;
    Rect::new(
        d.x + (rect.x - world.x) * sx,
        d.y + (rect.y - world.y) * sy,
        rect.w * sx,
        rect.h * sy,
    )
}

/// The inverse of [`into_screen`]: a live screen rect back into world space.
fn into_world(rect: Rect, desk: Option<Rect>, world: Rect) -> Rect {
    let Some(d) = desk else { return rect };
    if world.w <= 0.0 || world.h <= 0.0 || d.w <= 0.0 || d.h <= 0.0 {
        return rect;
    }
    if (d.x - world.x).abs() < 1.0
        && (d.y - world.y).abs() < 1.0
        && (d.w - world.w).abs() < 1.0
        && (d.h - world.h).abs() < 1.0
    {
        return rect;
    }
    let sx = world.w / d.w;
    let sy = world.h / d.h;
    Rect::new(
        world.x + (rect.x - d.x) * sx,
        world.y + (rect.y - d.y) * sy,
        rect.w * sx,
        rect.h * sy,
    )
}

/// Is this KeyPress the server repeating a held key?
///
/// The server auto-repeats a held key: on this host 11 presses arrive in a
/// one-second hold. Every command here is a discrete action, so the first press
/// acts and its repeats are dropped. The server marks a repeat exactly: it
/// sends a `KeyRelease` and a `KeyPress` carrying the *same* timestamp. So a
/// press is a repeat when the immediately preceding release has the same
/// keycode and the same timestamp. A deliberate press always follows a release
/// with a later timestamp and therefore fires; so does a different key.
pub fn is_auto_repeat(last_release: Option<(u8, u32)>, code: u8, time: u32) -> bool {
    last_release == Some((code, time))
}

fn build_keymap(x: &X11) -> Result<Vec<(u8, u32)>> {
    use x11rb::protocol::xproto::ConnectionExt as _;
    let min = x.conn.setup().min_keycode;
    let max = x.conn.setup().max_keycode;
    let count = max - min + 1;
    let mapping = x.conn.get_keyboard_mapping(min, count)?.reply()?;
    let per = mapping.keysyms_per_keycode as usize;
    let mut out = Vec::new();
    for (i, chunk) in mapping.keysyms.chunks(per).enumerate() {
        if let Some(sym) = chunk.first() {
            out.push((min + i as u8, *sym));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn layout_write_replaces_complete_file_and_cleans_failed_temporary_file() {
        let dir = std::env::temp_dir().join(format!(
            "pergola-layout-write-test-{}-{}",
            std::process::id(),
            NEXT_LAYOUT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layout.json");
        std::fs::write(&path, "old layout").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_layout_atomically(&path, "new complete layout").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new complete layout");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        // A replacement error leaves the target untouched and removes the
        // staged file, instead of truncating the only copy.
        let blocked = dir.join("directory-target");
        std::fs::create_dir(&blocked).unwrap();
        assert!(write_layout_atomically(&blocked, "cannot replace a directory").is_err());
        assert!(blocked.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Build a committed-state signature for the diff tests.
    fn sig(root: &str, placements: &[(u32, Placement, &str)], active: u32) -> EventSig {
        EventSig {
            root: root.to_string(),
            placements: placements
                .iter()
                .map(|(window, placement, title)| (*window, (placement.clone(), title.to_string())))
                .collect(),
            active,
            tag: None,
        }
    }

    fn kinds(records: &[serde_json::Value]) -> Vec<String> {
        records
            .iter()
            .map(|r| r["type"].as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn a_window_on_another_host_desktop_is_left_alone() {
        assert!(!on_another_desktop(Some(2), Some(2)));
        assert!(on_another_desktop(Some(2), Some(3)));
        // EWMH sticky windows are on every desktop, never foreign.
        assert!(!on_another_desktop(Some(2), Some(u32::MAX)));
        // The desktop does not advertise a number, or the window does not say:
        // do not guess, and keep the pre-existing behaviour.
        assert!(!on_another_desktop(None, Some(3)));
        assert!(!on_another_desktop(Some(2), None));
    }

    #[test]
    fn a_new_window_is_reported_as_docked_or_opened() {
        let before = sig("tree", &[(1, Placement::Docked("p-a".into()), "one")], 1);
        let after = sig(
            "tree",
            &[
                (1, Placement::Docked("p-a".into()), "one"),
                (2, Placement::Docked("p-a".into()), "two"),
                (3, Placement::Floating("p-c".into()), "three"),
            ],
            1,
        );
        let records = diff_events(&before, &after);
        assert_eq!(kinds(&records), vec!["dock", "window-opened"]);
        assert_eq!(records[0]["window"], 2);
        assert_eq!(records[0]["panel"], "p-a");
        assert_eq!(records[1]["window"], 3);
        assert_eq!(records[1]["title"], "three");
    }

    #[test]
    fn a_docked_window_that_starts_floating_is_reported_as_undocked() {
        let before = sig("tree", &[(7, Placement::Docked("p-a".into()), "seven")], 7);
        let after = sig("tree", &[(7, Placement::Floating("p-a".into()), "seven")], 7);
        let records = diff_events(&before, &after);
        assert_eq!(kinds(&records), vec!["undock"]);
        assert_eq!(records[0]["window"], 7);
    }

    #[test]
    fn a_window_that_disappears_is_reported_as_closed() {
        let before = sig(
            "tree",
            &[
                (1, Placement::Docked("p-a".into()), "one"),
                (2, Placement::Docked("p-a".into()), "two"),
            ],
            1,
        );
        let after = sig("tree", &[(1, Placement::Docked("p-a".into()), "one")], 1);
        let records = diff_events(&before, &after);
        assert_eq!(kinds(&records), vec!["window-closed"]);
        assert_eq!(records[0]["window"], 2);
        assert_eq!(records[0]["title"], "two");
    }

    #[test]
    fn a_tree_change_is_one_layout_record_and_a_focus_change_one_focus_record() {
        let before = sig(r#"{"id":"a"}"#, &[(1, Placement::Docked("p-a".into()), "one")], 1);
        let after = sig(r#"{"id":"b"}"#, &[(1, Placement::Docked("p-a".into()), "one")], 2);
        let records = diff_events(&before, &after);
        // Focus 2 is not in the placement map, so its title is empty rather
        // than a stale one.
        assert_eq!(kinds(&records), vec!["layout", "focus"]);
        assert_eq!(records[0]["tree"]["id"], "b");
        assert_eq!(records[1]["window"], 2);
        assert_eq!(records[1]["title"], "");
    }

    #[test]
    fn a_tag_switch_is_one_tag_record() {
        let before = sig("tree", &[], 1);
        let mut after = sig("tree", &[], 1);
        after.tag = Some("work".into());
        let records = diff_events(&before, &after);
        assert_eq!(kinds(&records), vec!["tag"]);
        assert_eq!(records[0]["name"], "work");
        // Back to "no tag": a bar needs that transition as much as the others.
        let back = diff_events(&after, &before);
        assert_eq!(kinds(&back), vec!["tag"]);
        assert!(back[0]["name"].is_null());
        // Same tag, no record.
        assert!(diff_events(&after, &after).is_empty());
    }

    #[test]
    fn an_unchanged_state_produces_no_records() {
        let state = sig("tree", &[(1, Placement::Docked("p-a".into()), "one")], 1);
        assert!(diff_events(&state, &state).is_empty());
    }

    fn record(seat: &str, panel: &str, window: u32) -> SeatRecord {
        SeatRecord {
            seat: seat.to_string(),
            panel: panel.to_string(),
            window,
            class: "XTerm".to_string(),
            title: "xterm".to_string(),
        }
    }

    #[test]
    fn an_activation_request_finds_the_seat_and_whether_its_view_is_hidden() {
        // s-a owns a plain panel; s-b and s-c share a tab group whose shown
        // view is s-b (`Panel::new` selects the first).
        let seats = vec![
            record("s-a", "p-a", 1),
            record("s-b", "p-b", 2),
            record("s-c", "p-b", 3),
        ];
        let tree = LayoutNode::split(
            "root",
            Axis::X,
            vec![
                LayoutNode::panel("p-a", vec!["s-a".into()]),
                LayoutNode::panel("p-b", vec!["s-b".into(), "s-c".into()]),
            ],
        );
        assert_eq!(
            activation_target(&seats, &tree, 1),
            Some(("s-a".to_string(), "p-a".to_string(), false))
        );
        // The shown view of the tab group needs no switch.
        assert_eq!(
            activation_target(&seats, &tree, 2),
            Some(("s-b".to_string(), "p-b".to_string(), false))
        );
        // A hidden view must be shown first.
        assert_eq!(
            activation_target(&seats, &tree, 3),
            Some(("s-c".to_string(), "p-b".to_string(), true))
        );
        // A window we do not manage is a stale pager entry or a popup.
        assert_eq!(activation_target(&seats, &tree, 99), None);
    }

    #[test]
    fn a_floating_window_activation_has_no_view_to_show() {
        let seats = vec![record("s-f", "p-f", 7)];
        let tree = LayoutNode::desktop();
        assert_eq!(
            activation_target(&seats, &tree, 7),
            Some(("s-f".to_string(), "p-f".to_string(), false))
        );
    }

    #[test]
    fn a_hint_line_is_three_pixels_across_the_narrow_axis() {
        // A vertical boundary: the band is a narrow strip; the line goes down
        // its middle, never as wide as the strip itself.
        let vertical = hint_line(Rect::new(100.0, 40.0, 6.0, 200.0));
        assert_eq!(vertical.w, 3);
        assert_eq!(vertical.h, 200);
        assert_eq!(vertical.x, 101); // 100 + (6 - 3) / 2
        // A horizontal boundary: the same, rotated.
        let horizontal = hint_line(Rect::new(40.0, 100.0, 200.0, 6.0));
        assert_eq!(horizontal.w, 200);
        assert_eq!(horizontal.h, 3);
        assert_eq!(horizontal.y, 101);
    }

    #[test]
    fn a_drop_frame_is_four_bars_and_none_of_them_covers_the_rect() {
        let rect = Rect::new(0.0, 0.0, 800.0, 450.0);
        let bars = frame_bars(rect);
        assert_eq!(bars.len(), 4);
        for (bar, _) in &bars {
            assert!(bar.w > 0 && bar.h > 0, "{bar:?}");
            // A bar is 3 px in one dimension, never a filled rectangle: the
            // window under the frame must stay visible.
            assert!(bar.w == 3 || bar.h == 3, "{bar:?}");
        }
        // Every edge of the rectangle is covered.
        assert!(bars.iter().any(|(b, _)| b.x == 0));
        assert!(bars.iter().any(|(b, _)| b.x + b.w == 800));
        assert!(bars.iter().any(|(b, _)| b.y == 0));
        assert!(bars.iter().any(|(b, _)| b.y + b.h == 450));
    }

    #[test]
    fn pointer_edge_only_fires_near_an_edge_and_prefers_the_nearest() {
        let screen = Rect::new(0.0, 0.0, 1600.0, 900.0);
        assert_eq!(pointer_edge(10.0, 450.0, screen, 70.0), Some(Direction::Left));
        assert_eq!(pointer_edge(1590.0, 450.0, screen, 70.0), Some(Direction::Right));
        assert_eq!(pointer_edge(800.0, 20.0, screen, 70.0), Some(Direction::Up));
        assert_eq!(pointer_edge(800.0, 880.0, screen, 70.0), Some(Direction::Down));
        // Middle of the screen: no drop target.
        assert_eq!(pointer_edge(800.0, 450.0, screen, 70.0), None);
        // A corner that is 5 px from both edges is a tie; the first edge in
        // the list wins, so the answer is deterministic rather than arbitrary.
        assert_eq!(pointer_edge(5.0, 895.0, screen, 70.0), Some(Direction::Left));
        // Two pixels from the bottom and five from the left: the nearer edge.
        assert_eq!(pointer_edge(5.0, 898.0, screen, 70.0), Some(Direction::Down));
    }

    #[test]
    fn a_held_key_is_one_press_not_eleven() {
        // The server repeats a held key ~11 times a second, each repeat as a
        // KeyRelease+KeyPress pair sharing one timestamp. A press whose
        // timestamp equals the preceding release's is that repeat.
        assert!(is_auto_repeat(Some((113, 92756959)), 113, 92756959));
        // The real press arrives with no preceding release, so it fires.
        assert!(!is_auto_repeat(None, 113, 92756919));
        // A deliberate second press follows a release with a later timestamp.
        assert!(!is_auto_repeat(Some((113, 92756959)), 113, 92757020));
        // A different key is never a repeat, even at the same timestamp.
        assert!(!is_auto_repeat(Some((113, 92756959)), 114, 92756959));
    }

    #[test]
    fn a_zoomed_boundary_uses_its_own_axis_scale() {
        // A split 1600x400 on a 1600x800 screen: x is 1:1, y doubles.
        let world = Rect::new(800.0, 0.0, 6.0, 400.0);
        let shown = Rect::new(800.0, 0.0, 6.0, 800.0);
        assert!((axis_scale(Axis::X, world, shown) - 1.0).abs() < 1e-9);
        assert!((axis_scale(Axis::Y, world, shown) - 2.0).abs() < 1e-9);
        // Degenerate bands fall back to 1 instead of dividing by zero.
        assert_eq!(axis_scale(Axis::X, Rect::new(0.0, 0.0, 0.0, 0.0), shown), 1.0);
    }

    #[test]
    fn untransform_undoes_the_camera() {
        let screen = Rect::new(0.0, 27.0, 1600.0, 873.0);
        let framed = Rect::new(400.0, 200.0, 800.0, 400.0);
        for rect in [
            Rect::new(400.0, 200.0, 800.0, 400.0),
            Rect::new(10.0, 900.0, 120.0, 60.0),
        ] {
            let shown = transform(rect, Some(framed), screen);
            let back = untransform(shown, Some(framed), screen);
            assert!((back.x - rect.x).abs() < 1e-6, "x {:?}", back);
            assert!((back.y - rect.y).abs() < 1e-6, "y {:?}", back);
            assert!((back.w - rect.w).abs() < 1e-6, "w {:?}", back);
            assert!((back.h - rect.h).abs() < 1e-6, "h {:?}", back);
        }
    }

    #[test]
    fn a_drop_near_an_edge_snaps_to_it() {
        let screen = Rect::new(0.0, 27.0, 1600.0, 873.0);
        let left = Rect::new(2.0, 300.0, 400.0, 300.0);
        let right = Rect::new(1198.0, 300.0, 400.0, 300.0);
        let top = Rect::new(500.0, 29.0, 400.0, 300.0);
        let bottom = Rect::new(500.0, 596.0, 400.0, 300.0);
        let middle = Rect::new(600.0, 300.0, 400.0, 300.0);
        assert_eq!(snap_edge(left, screen, EDGE_SNAP_PX), Some(Direction::Left));
        assert_eq!(snap_edge(right, screen, EDGE_SNAP_PX), Some(Direction::Right));
        assert_eq!(snap_edge(top, screen, EDGE_SNAP_PX), Some(Direction::Up));
        assert_eq!(snap_edge(bottom, screen, EDGE_SNAP_PX), Some(Direction::Down));
        assert_eq!(snap_edge(middle, screen, EDGE_SNAP_PX), None);
    }

    #[test]
    fn a_corner_drop_picks_the_nearer_edge() {
        let screen = Rect::new(0.0, 0.0, 1000.0, 800.0);
        // 3 px from the top, 9 px from the left: the top wins.
        let rect = Rect::new(9.0, 3.0, 200.0, 200.0);
        assert_eq!(snap_edge(rect, screen, EDGE_SNAP_PX), Some(Direction::Up));
    }

    #[test]
    fn a_single_report_that_travelled_is_a_drag() {
        // openbox reports the drop position once: one event, but 900 px of travel.
        let watch = MoveWatch {
            moves: 1,
            at: Instant::now(),
            rect: Rect::new(-156.0, 322.0, 316.0, 238.0),
            start: Rect::new(901.0, 322.0, 316.0, 238.0),
        };
        assert!(is_drag(&watch));
    }

    #[test]
    fn a_placement_at_the_edge_is_not_a_drag() {
        let placed = Rect::new(0.0, 0.0, 400.0, 300.0);
        // The WM clamped a new window to the corner and we never applied a rect:
        // no baseline, so no travel and no moves.
        let clamped = MoveWatch {
            moves: 0,
            at: Instant::now(),
            rect: placed,
            start: placed,
        };
        assert!(!is_drag(&clamped));
        // One event but a few px of placement jitter: still not a drag.
        let jitter = MoveWatch {
            moves: 1,
            at: Instant::now(),
            rect: placed,
            start: Rect::new(6.0, 3.0, 400.0, 300.0),
        };
        assert!(!is_drag(&jitter));
        // The event burst the xfwm4 path produces is still a drag.
        let burst = MoveWatch {
            moves: 3,
            at: Instant::now(),
            rect: placed,
            start: placed,
        };
        assert!(is_drag(&burst));
    }

    #[test]
    fn a_watch_overtaken_by_a_layout_apply_is_not_a_gesture() {
        // #231: a chord `dock left` moved the tile while a wmctrl move's watch
        // was still pending. The watch's rect is where the drag put it, the
        // live window is where the dock put it: acting on the former would
        // float a window the user had just docked.
        let recorded = Rect::new(400.0, 300.0, 510.0, 434.0);
        let live = Rect::new(0.0, 0.0, 794.0, 896.0);
        assert!(watch_overtaken(recorded, live));
        // A real drop reports the position the window is still at, so the
        // openbox single-event gesture keeps working.
        assert!(!watch_overtaken(recorded, recorded));
        // Rounding and decoration echoes are not moves: openbox reported y=452
        // for a window placed at 453 and the drop was discarded as stale.
        let rounded = Rect::new(400.2, 299.0, 510.2, 434.2);
        assert!(!watch_overtaken(recorded, rounded));
        // A one-pixel re-place is still the same gesture.
        assert!(!watch_overtaken(recorded, Rect::new(401.0, 300.0, 510.0, 434.0)));
    }

    #[test]
    fn an_echo_of_our_own_apply_is_discarded_even_when_deferred() {
        // The event said the window was still at the intermediate rect of a
        // two-step apply, but the live window ended up where we asked: echo.
        let applied = Rect::new(100.0, 100.0, 400.0, 300.0);
        let remembered = Rect::new(0.0, 0.0, 400.0, 300.0);
        let now = Instant::now();
        assert_eq!(
            deferred_action(Some(applied), remembered, applied, None, now),
            DeferredAction::Discard
        );
    }

    #[test]
    fn a_decoration_rounding_echo_is_still_our_own_apply() {
        // openbox reports the client at exactly the position we asked for, so
        // the frame rect computed from the report is 1 px left and 1 px up of
        // the one we recorded. That is our echo, not a drag: at a 0.5 px
        // tolerance every apply grew a phantom watch and the parked tile
        // re-docked into the tree (measured 2026-11-23, openbox e2e).
        let applied = Rect::new(803.0, 0.0, 792.0, 889.0);
        let reported = Rect::new(802.0, -1.0, 792.0, 889.0);
        assert!(!same_rect(applied, reported), "an exact compare says different");
        assert!(same_rect_within(applied, reported, DECORATION_SLOP_PX));
        let now = Instant::now();
        assert_eq!(
            deferred_action(Some(applied), reported, reported, None, now),
            DeferredAction::Discard
        );
        // A real move is still a move.
        assert!(!same_rect_within(applied, Rect::new(500.0, 0.0, 792.0, 889.0), DECORATION_SLOP_PX));
    }

    #[test]
    fn a_window_we_parked_off_screen_is_never_a_drag() {
        // The daemon parks a window below the work area; openbox pulls it back
        // so 44 px stay reachable, which is 44 px of travel — enough to read as
        // a drag and re-dock the parked tile.
        let screen = Rect::new(0.0, 0.0, 1600.0, 900.0);
        let parked = Rect::new(0.0, 900.0, 1596.0, 447.0);
        let clamped = Rect::new(0.0, 856.0, 1596.0, 447.0);
        assert!(parked_off_screen(parked, screen));
        assert!(!parked_off_screen(clamped, screen), "a clamped window is on screen");
        assert!(!parked_off_screen(Rect::new(0.0, 0.0, 792.0, 889.0), screen));
        // A zero-sized screen (no output geometry yet) parks nothing: without a
        // work area every rect "misses" it, and every configure would be
        // swallowed.
        assert!(overlaps(Rect::new(0.0, 0.0, 10.0, 10.0), screen));
        assert!(!overlaps(screen, Rect::new(0.0, 0.0, 0.0, 900.0)));
    }

    #[test]
    fn a_deferred_drop_is_taken_once_the_window_has_settled() {
        let applied = Rect::new(900.0, 300.0, 400.0, 300.0);
        let dropped = Rect::new(0.0, 300.0, 400.0, 300.0);
        let now = Instant::now();
        // First sight after the guard: live matches the event, clock starts.
        assert_eq!(
            deferred_action(Some(applied), dropped, dropped, None, now),
            DeferredAction::Wait
        );
        // Still inside the settle window: not judged yet.
        assert_eq!(
            deferred_action(
                Some(applied),
                dropped,
                dropped,
                Some(now),
                now + Duration::from_millis(40)
            ),
            DeferredAction::Wait
        );
        // Settled: the openbox drop is ready to be judged as a drag.
        assert_eq!(
            deferred_action(
                Some(applied),
                dropped,
                dropped,
                Some(now),
                now + DEFERRED_STABLE
            ),
            DeferredAction::Ready(dropped)
        );
    }

    #[test]
    fn a_window_still_moving_restarts_the_deferred_clock() {
        let remembered = Rect::new(500.0, 300.0, 400.0, 300.0);
        let live = Rect::new(300.0, 300.0, 400.0, 300.0);
        let now = Instant::now();
        assert_eq!(
            deferred_action(None, remembered, live, Some(now), now),
            DeferredAction::Restart(live)
        );
    }

    #[test]
    fn transform_is_identity_without_a_frame() {
        let r = Rect::new(10.0, 20.0, 30.0, 40.0);
        assert_eq!(transform(r, None, Rect::new(0.0, 0.0, 100.0, 100.0)), r);
    }

    #[test]
    fn transform_fills_the_screen_with_the_framed_node() {
        let screen = Rect::new(0.0, 0.0, 1000.0, 500.0);
        let framed = Rect::new(100.0, 50.0, 500.0, 250.0);
        let out = transform(framed, Some(framed), screen);
        assert!((out.x - 0.0).abs() < 1e-9);
        assert!((out.y - 0.0).abs() < 1e-9);
        assert!((out.w - 1000.0).abs() < 1e-9);
        assert!((out.h - 500.0).abs() < 1e-9);
    }

    #[test]
    fn transform_keeps_relative_positions() {
        let screen = Rect::new(0.0, 0.0, 1000.0, 500.0);
        let framed = Rect::new(0.0, 0.0, 500.0, 250.0);
        let sibling = Rect::new(500.0, 0.0, 500.0, 250.0);
        let out = transform(sibling, Some(framed), screen);
        assert!((out.x - 1000.0).abs() < 1e-9);
        assert!((out.w - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn transform_fills_the_work_area_not_the_display() {
        // 1600x900 display with a 27 px top panel: the tile must land at y=27
        // and be 873 tall, or the bottom of every window sits off-screen.
        let work = Rect::new(0.0, 27.0, 1600.0, 873.0);
        let framed = Rect::new(0.0, 27.0, 800.0, 873.0);
        let out = transform(framed, Some(framed), work);
        assert!((out.y - 27.0).abs() < 1e-9);
        assert!((out.h - 873.0).abs() < 1e-9);
        let right = Rect::new(808.0, 27.0, 800.0, 873.0);
        let out = transform(right, Some(framed), work);
        assert!((out.x - 1616.0).abs() < 1e-9);
    }

    #[test]
    fn desktop_mapping_round_trips_in_an_offset_work_area() {
        let world = Rect::new(0.0, 27.0, 1600.0, 873.0);
        let desk = Rect::new(0.0, 27.0, 1600.0, 873.0);
        let rect = Rect::new(100.0, 200.0, 400.0, 300.0);
        assert_eq!(into_screen(rect, Some(desk), world), rect);
        assert_eq!(into_world(rect, Some(desk), world), rect);
    }

    #[test]
    fn transform_fills_both_axes() {
        let screen = Rect::new(0.0, 0.0, 1000.0, 500.0);
        let framed = Rect::new(100.0, 100.0, 200.0, 100.0);
        let out = transform(framed, Some(framed), screen);
        assert!((out.x).abs() < 1e-9);
        assert!((out.y).abs() < 1e-9);
        assert!((out.w - 1000.0).abs() < 1e-9);
        assert!((out.h - 500.0).abs() < 1e-9);
    }

    #[test]
    fn desktop_mapping_round_trips() {
        let world = Rect::new(0.0, 0.0, 1600.0, 900.0);
        let desk = Rect::new(804.0, 0.0, 796.0, 900.0);
        let rect = Rect::new(100.0, 200.0, 400.0, 300.0);
        let screen = into_screen(rect, Some(desk), world);
        assert!((screen.x - (804.0 + 100.0 * 796.0 / 1600.0)).abs() < 1e-9);
        assert!((screen.w - 400.0 * 796.0 / 1600.0).abs() < 1e-9);
        let back = into_world(screen, Some(desk), world);
        assert!((back.x - rect.x).abs() < 1e-6);
        assert!((back.y - rect.y).abs() < 1e-6);
        assert!((back.w - rect.w).abs() < 1e-6);
        assert!((back.h - rect.h).abs() < 1e-6);
    }

    #[test]
    fn full_screen_desktop_is_identity() {
        let world = Rect::new(0.0, 0.0, 1600.0, 900.0);
        let rect = Rect::new(10.0, 20.0, 30.0, 40.0);
        assert_eq!(into_screen(rect, Some(world), world), rect);
        assert_eq!(into_world(rect, Some(world), world), rect);
    }

    #[test]
    fn overlay_lines_sit_in_the_gaps() {
        let engine = LayoutEngine::new(8.0, 0.05);
        let tree = LayoutNode::split(
            "root",
            Axis::X,
            vec![
                LayoutNode::panel("p-a", vec!["s-a".into()]),
                LayoutNode::panel("p-b", vec!["s-b".into()]),
            ],
        );
        let world = Rect::new(0.0, 0.0, 1000.0, 500.0);
        let rects = engine.rects(&tree, world);
        let bars = overlay_bars(&tree, &rects, None, 8.0, world, false);
        assert_eq!(bars.len(), 1, "one boundary for two children");
        let (bar, _) = bars[0];
        // a is 0..496, the gap is 496..504, so the line is centred at 500.
        assert_eq!(bar.x, 499);
        assert_eq!(bar.w, 2);
        assert_eq!(bar.h, 500);
        // No bar may overlap a tile: the line is inside the gap.
        assert!(bar.x >= 496 && bar.x + bar.w <= 504);
    }

    #[test]
    fn a_line_is_four_pixels_when_it_has_to_carry_the_cue() {
        // With no dim layer the split line is the only thing that says "this is
        // a boundary", so it is drawn 4 px wide inside the same gap.
        let e = LayoutEngine::new(8.0, 0.05);
        let tree = LayoutNode::split(
            "r",
            Axis::X,
            vec![
                LayoutNode::panel("p-a", vec!["s-a".into()]),
                LayoutNode::panel("p-b", vec!["s-b".into()]),
            ],
        );
        let world = Rect::new(0.0, 0.0, 1000.0, 500.0);
        let rects = e.rects(&tree, world);
        let thin = overlay_bars(&tree, &rects, None, 8.0, world, false);
        let wide = overlay_bars(&tree, &rects, None, 8.0, world, true);
        assert_eq!(thin.len(), wide.len(), "the cue is width, not count");
        assert_eq!(thin[0].0.w, 2);
        assert_eq!(wide[0].0.w, 4);
        // Both stay inside the gap (496..504): the wider line is still a
        // divider, never a border over a tile.
        for (bar, _) in &wide {
            assert!(bar.x >= 496 && bar.x + bar.w <= 504, "{bar:?}");
        }
    }

    #[test]
    fn the_dim_percentage_follows_the_config_value() {
        assert_eq!(dim_percent(0.4), 40);
        assert_eq!(dim_percent(0.0), 0);
        assert_eq!(dim_percent(0.65), 65);
        // Out-of-range values are clamped, so a reply never claims 240%.
        assert_eq!(dim_percent(2.4), 90);
    }

    #[test]
    fn overlay_marks_the_framed_path_and_the_viewport() {
        let engine = LayoutEngine::new(8.0, 0.05);
        let inner = LayoutNode::split(
            "inner",
            Axis::Y,
            vec![
                LayoutNode::panel("p-b", vec!["s-b".into()]),
                LayoutNode::panel("p-c", vec!["s-c".into()]),
            ],
        );
        let tree = LayoutNode::split(
            "root",
            Axis::X,
            vec![LayoutNode::panel("p-a", vec!["s-a".into()]), inner],
        );
        let world = Rect::new(0.0, 0.0, 1000.0, 500.0);
        let rects = engine.rects(&tree, world);
        let bars = overlay_bars(&tree, &rects, Some("p-c"), 8.0, world, false);
        // root boundary + inner boundary + 4 viewport edges
        assert_eq!(bars.len(), 6);
        let accent = bars.iter().filter(|(_, c)| *c == 0x00e8b04b).count();
        assert_eq!(accent, 6, "both splits are on the path, plus the frame");
    }

    #[test]
    fn windows_outside_the_view_are_parked_below_it() {
        let view = IRect::new(0, 27, 1600, 873);
        // fully to the right of the framed node
        let right = IRect::new(1700, 27, 400, 400);
        assert_eq!(park_if_outside(right, view, true), IRect::new(0, 900, 400, 400));
        // fully above
        let above = IRect::new(0, -500, 400, 400);
        assert_eq!(park_if_outside(above, view, true), IRect::new(0, 900, 400, 400));
        // partially visible stays put: a zoom may legitimately clip a tile
        let partial = IRect::new(1500, 27, 400, 400);
        assert_eq!(park_if_outside(partial, view, true), partial);
        // not zoomed: never park
        assert_eq!(park_if_outside(right, view, false), right);
    }

    #[test]
    fn window_ids_parse_in_both_bases() {
        assert_eq!(parse_window("0x1a2b"), Some(0x1a2b));
        assert_eq!(parse_window("6699"), Some(6699));
        assert_eq!(parse_window("nope"), None);
    }
}
