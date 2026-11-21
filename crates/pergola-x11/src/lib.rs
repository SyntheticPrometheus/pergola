//! X11/EWMH backend.
//!
//! Pergola is not an X window manager: it never takes `SubstructureRedirect` on
//! the root window, so it runs *inside* whatever desktop is already there
//! (xfwm4, mutter, kwin_x11, i3, openbox, …). It reads the EWMH client list and
//! asks the running WM to move windows with `_NET_MOVERESIZE_WINDOW`, exactly
//! the way `wmctrl` does.

pub mod tag_parking;

use std::collections::BTreeSet;

use anyhow::{anyhow, Context, Result};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt as _, CreateWindowAux, Cursor, EventMask, GrabMode, ModMask, PropMode, Window,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use pergola_core::{Axis, IRect, Rect};

/// A managed top-level window, as Pergola sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub id: u32,
    /// Client-area rectangle in root coordinates.
    pub rect: IRect,
    /// Frame extents (left, right, top, bottom) reported by the WM.
    pub extents: (i32, i32, i32, i32),
    pub title: String,
    pub class: String,
    pub pid: Option<u32>,
    /// `_NET_WM_STATE_HIDDEN` (minimised or on another desktop).
    pub hidden: bool,
    pub active: bool,
    /// `WM_TRANSIENT_FOR` is set: a dialog or popup owned by another window.
    /// Such windows are never tiled (see `docs/HISTORY-LESSONS.md`).
    pub transient: bool,
    /// `_NET_WM_STATE_FULLSCREEN`: the WM owns this window's geometry.
    pub fullscreen: bool,
    /// `_NET_WM_STATE_MAXIMIZED_{HORZ,VERT}`: a move needs the state dropped
    /// first, because a maximized window ignores `_NET_MOVERESIZE_WINDOW`.
    pub maximized: bool,
}

/// The ICCCM `WM_NORMAL_HINTS` fields that constrain a tiled window.
///
/// A terminal asks for a client area of `base + n * inc` pixels: anything else
/// and the WM clamps the window, which lands it an arbitrary distance from the
/// tile it was meant to fill (the 11 px shortfall measured under Openbox). The
/// tiler's answer is to snap the *client* area down to the client's own grid and
/// let the remainder become a gap at the tile's right or bottom edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SizeHints {
    pub base_w: i32,
    pub base_h: i32,
    pub inc_w: i32,
    pub inc_h: i32,
    pub min_w: i32,
    pub min_h: i32,
    pub max_w: i32,
    pub max_h: i32,
}

impl SizeHints {
    /// True when the client draws a character grid and expects increments.
    pub fn has_increments(&self) -> bool {
        self.inc_w > 1 || self.inc_h > 1
    }
}

/// Fit one client dimension to the client's own resize grid, never exceeding
/// `available`. `inc <= 1` means "no grid": the available size is used as-is,
/// clamped by min/max.
pub fn snap_client_size(available: i32, base: i32, inc: i32, min: i32, max: i32) -> i32 {
    let available = available.max(1);
    let mut size = if inc > 1 {
        let base = base.max(0);
        let steps = ((available - base) / inc).max(0);
        base + steps * inc
    } else {
        available
    };
    if min > 0 {
        size = size.max(min);
    }
    if max > 0 {
        size = size.min(max);
    }
    size.max(1)
}

/// Snap a frame rectangle so its client area fits the client's size grid.
/// The frame origin is kept, so the leftover pixels appear at the tile's right
/// or bottom edge, where the next tile starts.
pub fn snap_to_hints(
    frame: IRect,
    hints: SizeHints,
    extents: (i32, i32, i32, i32),
) -> IRect {
    if !hints.has_increments() {
        return frame;
    }
    let (l, r, t, b) = extents;
    let client_w = snap_client_size(frame.w - l - r, hints.base_w, hints.inc_w, hints.min_w, hints.max_w);
    let client_h = snap_client_size(frame.h - t - b, hints.base_h, hints.inc_h, hints.min_h, hints.max_h);
    IRect::new(frame.x, frame.y, client_w + l + r, client_h + t + b)
}

impl WindowInfo {
    /// The rectangle including WM decorations, which is what the user sees.
    pub fn frame_rect(&self) -> IRect {
        let (l, r, t, b) = self.extents;
        IRect::new(
            self.rect.x - l,
            self.rect.y - t,
            self.rect.w + l + r,
            self.rect.h + t + b,
        )
    }
}

/// A key binding: modifiers plus an X keysym name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    pub mods: u16,
    pub keysym: u32,
    pub label: String,
}

pub struct X11 {
    pub conn: RustConnection,
    pub screen: usize,
    pub root: Window,
    atoms: Atoms,
    overlay: Vec<Window>,
    /// The dim layer drawn under the overlay's split lines when a compositor
    /// can render it. Kept apart from `overlay` so `overlay off` can prove it
    /// destroyed the layer and callers can report its id.
    overlay_dim: Option<Window>,
    /// Invisible input-only windows that turn each split gap into a pointer
    /// drag target. Kept so a layout change can replace them as a set.
    bands: Vec<Window>,
    /// Pointer affordances: a highlighted gap band on hover, the frame a
    /// dragged window will land in. Separate from `overlay` so a hint never
    /// changes what `overlay on`/`off` means and `overlay off` cannot leave one
    /// behind.
    hints: Vec<Window>,
    /// Resize cursors for horizontal and vertical boundaries, when the X
    /// server has the standard `cursor` font.
    cursors: (Cursor, Cursor),
}

struct Atoms {
    net_client_list: u32,
    net_active_window: u32,
    net_wm_name: u32,
    net_wm_pid: u32,
    net_wm_window_type: u32,
    net_wm_state: u32,
    net_wm_state_hidden: u32,
    net_wm_state_maximized_horz: u32,
    net_wm_state_maximized_vert: u32,
    net_wm_state_fullscreen: u32,
    net_frame_extents: u32,
    net_workarea: u32,
    net_current_desktop: u32,
    /// `_NET_WM_DESKTOP`: which host desktop a window is on. The host WM's
    /// workspaces are not pergola's tags — a window the user moved to another
    /// desktop belongs to the WM there, and moving it from here drags it back.
    net_wm_desktop: u32,
    net_moveresize_window: u32,
    /// `_NET_WM_CM_S<screen>` — the selection a compositor owns while it is
    /// running (xfwm4, picom, kwin all follow the freedesktop convention).
    net_wm_cm: u32,
    net_wm_window_opacity: u32,
    utf8_string: u32,
    wm_class: u32,
    type_skip: Vec<u32>,
}

impl X11 {
    pub fn connect() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context(
            "cannot open the X display (is DISPLAY set? Pergola needs an X11 session)",
        )?;
        let root = conn.setup().roots[screen].root;
        let atoms = Atoms::new(&conn, screen)?;
        let cursors = make_resize_cursors(&conn).unwrap_or((0, 0));
        Ok(Self {
            conn,
            screen,
            root,
            atoms,
            overlay: Vec::new(),
            overlay_dim: None,
            bands: Vec::new(),
            hints: Vec::new(),
            cursors,
        })
    }

    /// The whole display: always starts at 0,0 and ignores panels.
    pub fn screen_rect(&self) -> Result<IRect> {
        let g = self.conn.get_geometry(self.root)?.reply()?;
        Ok(IRect::new(0, 0, g.width as i32, g.height as i32))
    }

    /// The area the window manager leaves for managed windows: `_NET_WORKAREA`
    /// for the current desktop, so panels and docks are not tiled over. Falls
    /// back to [`Self::screen_rect`] when the property is missing or degenerate.
    pub fn workarea_rect(&self) -> Result<IRect> {
        let screen = self.screen_rect()?;
        let desktop = self
            .cardinal_prop(self.root, self.atoms.net_current_desktop)
            .and_then(|v| v.first().copied())
            .unwrap_or(0) as usize;
        Ok(pick_workarea(
            self.cardinal_prop(self.root, self.atoms.net_workarea),
            desktop,
            screen,
        ))
    }

    /// EWMH `_NET_CLIENT_LIST`, in stacking order.
    pub fn client_list(&self) -> Result<Vec<u32>> {
        let reply = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.net_client_list,
                AtomEnum::WINDOW,
                0,
                4096,
            )?
            .reply()?;
        Ok(reply.value32().map(|it| it.collect()).unwrap_or_default())
    }

    pub fn active_window(&self) -> Option<u32> {
        let reply = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.net_active_window,
                AtomEnum::WINDOW,
                0,
                1,
            )
            .ok()?
            .reply()
            .ok()?;
        let value = reply.value32().and_then(|mut it| it.next());
        value
    }

    /// Describe one window, or `None` when it is not a normal managed window.
    pub fn info(&self, win: u32) -> Result<Option<WindowInfo>> {
        let attrs = self.conn.get_window_attributes(win)?.reply()?;
        if attrs.override_redirect || attrs.map_state != x11rb::protocol::xproto::MapState::VIEWABLE
        {
            return Ok(None);
        }
        let types = self.atom_list(win, self.atoms.net_wm_window_type)?;
        if types.iter().any(|t| self.atoms.type_skip.contains(t)) {
            return Ok(None);
        }
        let states = self.atom_list(win, self.atoms.net_wm_state)?;
        let hidden = states.contains(&self.atoms.net_wm_state_hidden);
        let fullscreen = states.contains(&self.atoms.net_wm_state_fullscreen);
        let maximized = states.contains(&self.atoms.net_wm_state_maximized_horz)
            || states.contains(&self.atoms.net_wm_state_maximized_vert);
        let transient = self
            .conn
            .get_property(false, win, AtomEnum::WM_TRANSIENT_FOR, AtomEnum::WINDOW, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.value_len > 0)
            .unwrap_or(false);

        let geom = self.conn.get_geometry(win)?.reply()?;
        let translated = self
            .conn
            .translate_coordinates(win, self.root, 0, 0)?
            .reply()?;
        let rect = IRect::new(
            translated.dst_x as i32,
            translated.dst_y as i32,
            geom.width as i32,
            geom.height as i32,
        );

        let extents = self.frame_extents(win);

        let title = self
            .string_prop(win, self.atoms.net_wm_name)
            .or_else(|| self.string_prop(win, AtomEnum::WM_NAME.into()))
            .unwrap_or_default();
        let class = self
            .string_prop(win, self.atoms.wm_class)
            .unwrap_or_default()
            .split('\0')
            .next_back()
            .unwrap_or("")
            .to_string();
        let pid = self
            .conn
            .get_property(false, win, self.atoms.net_wm_pid, AtomEnum::CARDINAL, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut it| it.next()));

        Ok(Some(WindowInfo {
            id: win,
            rect,
            extents,
            title,
            class,
            pid,
            hidden,
            active: self.active_window() == Some(win),
            transient,
            fullscreen,
            maximized,
        }))
    }

    /// Every normal managed window, in stacking order.
    pub fn windows(&self) -> Result<Vec<WindowInfo>> {
        let mut out = Vec::new();
        for win in self.client_list()? {
            if let Ok(Some(info)) = self.info(win) {
                out.push(info);
            }
        }
        Ok(out)
    }

    /// The host desktop the WM is showing (`_NET_CURRENT_DESKTOP`), or `None`
    /// when the desktop does not advertise one.
    pub fn current_desktop(&self) -> Option<u32> {
        self.cardinal_prop(self.root, self.atoms.net_current_desktop)?
            .into_iter()
            .next()
    }

    /// The host desktop a window is on (`_NET_WM_DESKTOP`). `Some(0xFFFFFFFF)`
    /// is the EWMH "on every desktop" value; `None` means the property is
    /// missing, so the caller cannot tell and should not assume.
    pub fn desktop_of(&self, win: u32) -> Option<u32> {
        self.cardinal_prop(win, self.atoms.net_wm_desktop)?
            .into_iter()
            .next()
    }

    /// Ask the running WM to place `win` so that its *frame* occupies `frame`.
    ///
    /// `_NET_MOVERESIZE_WINDOW` is the EWMH way for an external tool to move a
    /// window: the WM keeps its own gravity and decoration bookkeeping. Gravity
    /// is NorthWest (0), so x/y are the frame origin, and the width/height are
    /// client dimensions — hence the frame extents are subtracted here.
    pub fn move_resize(&self, win: u32, frame: IRect) -> Result<()> {
        let (l, r, t, b) = self.frame_extents(win);
        let gravity = 0u32; // NorthWestGravity
        let flags = 0xFu32; // x, y, w, h all present
        let source = 2u32; // pager
        let data = [
            gravity | (flags << 8) | (source << 12),
            frame.x as u32,
            frame.y as u32,
            (frame.w - l - r).max(1) as u32,
            (frame.h - t - b).max(1) as u32,
        ];
        if std::env::var_os("PERGOLA_DEBUG").is_some() {
            eprintln!(
                "pergola: move 0x{win:x} frame={:?} extents={:?} data={:?}",
                frame, (l, r, t, b), data
            );
        }
        let event = ClientMessageEvent::new(32, win, self.atoms.net_moveresize_window, data);
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// Direct configure of the client area, used as a fallback.
    pub fn configure(&self, win: u32, rect: IRect) -> Result<()> {
        self.conn.configure_window(
            win,
            &ConfigureWindowAux::new()
                .x(rect.x)
                .y(rect.y)
                .width(rect.w.max(1) as u32)
                .height(rect.h.max(1) as u32),
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// `_NET_FRAME_EXTENTS` (left, right, top, bottom), or zeros.
    pub fn frame_extents(&self, win: u32) -> (i32, i32, i32, i32) {
        self.conn
            .get_property(
                false,
                win,
                self.atoms.net_frame_extents,
                AtomEnum::CARDINAL,
                0,
                4,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| {
                let v: Vec<u32> = r.value32()?.collect();
                (v.len() == 4).then(|| (v[0] as i32, v[1] as i32, v[2] as i32, v[3] as i32))
            })
            .unwrap_or((0, 0, 0, 0))
    }

    /// The client's own resize grid (`WM_NORMAL_HINTS`), all zero when the
    /// window does not ask for one. Read through the `WM_SIZE_HINTS` type, as
    /// ICCCM requires; a property of any other type is ignored.
    pub fn size_hints(&self, win: u32) -> SizeHints {
        let reply = self
            .conn
            .get_property(
                false,
                win,
                AtomEnum::WM_NORMAL_HINTS,
                AtomEnum::WM_SIZE_HINTS,
                0,
                18,
            )
            .ok()
            .and_then(|c| c.reply().ok());
        let Some(reply) = reply else {
            return SizeHints::default();
        };
        let v: Vec<u32> = reply.value32().map(|it| it.collect()).unwrap_or_default();
        if v.len() < 18 {
            return SizeHints::default();
        }
        let flags = v[0];
        let field = |bit: u32, index: usize| -> i32 {
            if flags & bit != 0 {
                v[index] as i32
            } else {
                0
            }
        };
        // ICCCM WM_SIZE_HINTS flags: PMinSize 1<<4, PMaxSize 1<<5,
        // PResizeInc 1<<6, PBaseSize 1<<8.
        SizeHints {
            base_w: field(1 << 8, 15),
            base_h: field(1 << 8, 16),
            inc_w: field(1 << 6, 9),
            inc_h: field(1 << 6, 10),
            min_w: field(1 << 4, 5),
            min_h: field(1 << 4, 6),
            max_w: field(1 << 5, 7),
            max_h: field(1 << 5, 8),
        }
    }

    /// Whether the window is in `_NET_WM_STATE_FULLSCREEN`. A fullscreen window
    /// owns its own geometry: the tiler must leave it alone.
    pub fn fullscreen(&self, win: u32) -> bool {
        let Ok(states) = self.atom_list(win, self.atoms.net_wm_state) else {
            return false;
        };
        states.contains(&self.atoms.net_wm_state_fullscreen)
    }

    pub fn activate(&self, win: u32) -> Result<()> {
        let data = [2u32, 0, 0, 0, 0]; // source = pager, CurrentTime
        let event = ClientMessageEvent::new(32, win, self.atoms.net_active_window, data);
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// Whether the WM has the window maximized. A maximized window ignores
    /// `_NET_MOVERESIZE_WINDOW`, so docking it silently does nothing.
    pub fn maximized(&self, win: u32) -> bool {
        let Ok(states) = self.atom_list(win, self.atoms.net_wm_state) else {
            return false;
        };
        states.contains(&self.atoms.net_wm_state_maximized_horz)
            || states.contains(&self.atoms.net_wm_state_maximized_vert)
    }

    /// Ask the WM to drop the maximized state, so a following move takes.
    pub fn unmaximize(&self, win: u32) -> Result<()> {
        // EWMH `_NET_WM_STATE` client message: action 0 = remove, 2 properties.
        let data = [
            0u32,
            self.atoms.net_wm_state_maximized_horz,
            self.atoms.net_wm_state_maximized_vert,
            2u32,
            0u32,
        ];
        let event = ClientMessageEvent::new(32, win, self.atoms.net_wm_state, data);
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// Raise a window without focusing it.
    pub fn raise(&self, win: u32) -> Result<()> {
        self.conn.configure_window(
            win,
            &ConfigureWindowAux::new().stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
        )?;
        self.conn.flush()?;
        Ok(())
    }

    // MARK: overlay

    /// True when a compositor owns the `_NET_WM_CM_S<screen>` selection. That
    /// selection is the only portable signal: it is set by xfwm4's compositor,
    /// picom, kwin and mutter alike, and absent when compositing is off.
    pub fn compositor_active(&self) -> bool {
        self.conn
            .get_selection_owner(self.atoms.net_wm_cm)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.owner != x11rb::NONE)
            .unwrap_or(false)
    }

    /// True when the server has the SHAPE extension. Without it a full-screen
    /// dim layer would swallow every click meant for the tiles under it, so the
    /// daemon must not create one.
    pub fn shape_available(&self) -> bool {
        matches!(
            self.conn
                .extension_information(x11rb::protocol::shape::X11_EXTENSION_NAME),
            Ok(Some(_))
        )
    }

    /// Draw the overlay: an optional dim layer first (so the split lines sit on
    /// top of it), then filled bars each with its own colour. The daemon uses
    /// this for split lines that live in the gaps and for the viewport frame, so
    /// the overlay never covers a window's own decorations.
    ///
    /// `dim` is `(rect, opacity)` with opacity in the X convention — a 32-bit
    /// CARDINAL where `0xffffffff` is opaque. It is only drawn when the caller
    /// has already established that a compositor can render it.
    pub fn overlay_show(&mut self, bars: &[(IRect, u32)], dim: Option<(IRect, u32)>) -> Result<()> {
        self.overlay_hide()?;
        let screen = &self.conn.setup().roots[self.screen];
        let root_visual = screen.root_visual;
        let depth = screen.root_depth;
        if let Some((rect, opacity)) = dim {
            if rect.w > 0 && rect.h > 0 {
                let wid = self.conn.generate_id()?;
                self.conn.create_window(
                    depth,
                    wid,
                    self.root,
                    rect.x as i16,
                    rect.y as i16,
                    rect.w as u16,
                    rect.h as u16,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    root_visual,
                    &CreateWindowAux::new()
                        .background_pixel(0x000000)
                        .override_redirect(1)
                        .event_mask(EventMask::EXPOSURE),
                )?;
                // Set the opacity before mapping: a compositor reads the
                // property on the first frame, and setting it afterwards would
                // flash a fully opaque black rectangle over the whole screen.
                self.conn.change_property32(
                    PropMode::REPLACE,
                    wid,
                    self.atoms.net_wm_window_opacity,
                    AtomEnum::CARDINAL,
                    &[opacity],
                )?;
                // A full-screen INPUT_OUTPUT window would intercept every click
                // meant for the tiles. An empty input shape makes it a pure
                // drawing surface: pointer events fall through to what is below.
                let _ = x11rb::protocol::shape::rectangles(
                    &self.conn,
                    x11rb::protocol::shape::SO::SET,
                    x11rb::protocol::shape::SK::INPUT,
                    x11rb::protocol::xproto::ClipOrdering::UNSORTED,
                    wid,
                    0,
                    0,
                    &[],
                );
                self.conn.map_window(wid)?;
                self.conn.configure_window(
                    wid,
                    &ConfigureWindowAux::new()
                        .stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
                )?;
                self.overlay_dim = Some(wid);
            }
        }
        // Tag each bar so a test (and a debugging session) can tell an overlay
        // divider from any other override-redirect rectangle without guessing at
        // geometry: scripts/xhelper.py measures these. The hint layer uses its
        // own atom, so `overlay on/off` never confuses the two.
        let atom = self.atom("_PERGOLA_OVERLAY")?;
        let mut ids = Vec::new();
        for (bar, color) in bars {
            if bar.w <= 0 || bar.h <= 0 {
                continue;
            }
            let wid = self.create_bar(*bar, *color, false)?;
            // Record the window before the property write: if that write fails,
            // the window is still in the list, so the next `overlay_hide`
            // destroys it instead of leaking it until the connection closes.
            ids.push(wid);
            self.conn.change_property32(
                PropMode::REPLACE,
                wid,
                atom,
                AtomEnum::CARDINAL,
                &[1],
            )?;
        }
        self.conn.flush()?;
        self.overlay = ids;
        Ok(())
    }

    pub fn overlay_hide(&mut self) -> Result<()> {
        if let Some(wid) = self.overlay_dim.take() {
            let _ = self.conn.destroy_window(wid);
        }
        for wid in self.overlay.drain(..) {
            let _ = self.conn.destroy_window(wid);
        }
        self.conn.flush()?;
        Ok(())
    }

    pub fn overlay_active(&self) -> bool {
        !self.overlay.is_empty()
    }

    /// The dim layer's window id while it is mapped, so a caller can report it
    /// and a test can prove the layer was destroyed on `overlay off`.
    pub fn overlay_dim(&self) -> Option<Window> {
        self.overlay_dim
    }

    // MARK: pointer affordances

    /// One mapped, override-redirect rectangle filled with `color`, raised
    /// above the tiles. The overlay and the hint layer are both made of these,
    /// so they look identical and neither can be mistaken for a client window.
    fn create_bar(&self, rect: IRect, color: u32, pass_through: bool) -> Result<Window> {
        let screen = &self.conn.setup().roots[self.screen];
        let wid = self.conn.generate_id()?;
        self.conn.create_window(
            screen.root_depth,
            wid,
            self.root,
            rect.x as i16,
            rect.y as i16,
            rect.w as u16,
            rect.h as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(color)
                .override_redirect(1)
                .event_mask(EventMask::EXPOSURE),
        )?;
        if pass_through {
            // A hint sits exactly where the pointer is (the hovered gap is
            // under the pointer by definition). Without an empty input shape
            // the hint would take the pointer away from the band window, whose
            // LeaveNotify would then destroy the hint, which would hand the
            // pointer back: an enter/leave storm at every hover.
            let _ = x11rb::protocol::shape::rectangles(
                &self.conn,
                x11rb::protocol::shape::SO::SET,
                x11rb::protocol::shape::SK::INPUT,
                x11rb::protocol::xproto::ClipOrdering::UNSORTED,
                wid,
                0,
                0,
                &[],
            );
        }
        self.conn.map_window(wid)?;
        // Override-redirect windows are unmanaged, so the WM will not restack
        // them; raise them explicitly to sit above the tiles.
        self.conn.configure_window(
            wid,
            &ConfigureWindowAux::new()
                .stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
        )?;
        Ok(wid)
    }

    /// Replace the pointer hints with these rectangles. The daemon calls this
    /// when the pointer enters a draggable gap, and while a window is being
    /// dragged, to show where it will land.
    pub fn hint_show(&mut self, bars: &[(IRect, u32)]) -> Result<()> {
        self.hint_hide()?;
        // Tag the window so a test (and a debugging session) can tell a hint
        // from any other override-redirect rectangle on the screen without
        // guessing at geometry: scripts/xhelper.py counts these.
        let atom = self.atom("_PERGOLA_HINT")?;
        for (bar, color) in bars {
            if bar.w <= 0 || bar.h <= 0 {
                continue;
            }
            let wid = self.create_bar(*bar, *color, true)?;
            // Record the window before the property write: if that write fails,
            // the window is still in the list, so the next `hint_hide` destroys
            // it instead of leaking it until the connection closes.
            self.hints.push(wid);
            self.conn.change_property32(
                PropMode::REPLACE,
                wid,
                atom,
                AtomEnum::CARDINAL,
                &[1],
            )?;
        }
        self.conn.flush()?;
        Ok(())
    }

    pub fn hint_hide(&mut self) -> Result<()> {
        for wid in self.hints.drain(..) {
            let _ = self.conn.destroy_window(wid);
        }
        self.conn.flush()?;
        Ok(())
    }

    /// The pointer's root coordinates. During a WM-driven move the pointer is
    /// what says which edge the user is aiming at, so the drop hint follows it
    /// rather than the window's lagging ConfigureNotify stream.
    pub fn pointer(&self) -> Result<(i32, i32)> {
        let reply = self.conn.query_pointer(self.root)?.reply()?;
        Ok((reply.root_x as i32, reply.root_y as i32))
    }

    // MARK: pointer grab areas in the gaps

    /// Replace the pointer grab areas with one input-only window per gap band.
    ///
    /// These are invisible: they exist so the user can drag a split boundary
    /// with the mouse. They are created on top and are input-only, so they
    /// never cover a tile's pixels; each carries the matching resize cursor.
    /// Returns the window ids in the same order as `bands`.
    pub fn grab_bands_show(&mut self, bands: &[(IRect, Axis)]) -> Result<Vec<Window>> {
        self.grab_bands_hide()?;
        let (cursor_h, cursor_v) = self.cursors;
        let mut ids = Vec::new();
        for (rect, axis) in bands {
            if rect.w <= 0 || rect.h <= 0 {
                continue;
            }
            let wid = self.conn.generate_id()?;
            let cursor = match axis {
                Axis::X => cursor_h,
                Axis::Y => cursor_v,
            };
            let mut aux = CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(
                    EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::BUTTON_MOTION
                        | EventMask::ENTER_WINDOW
                        | EventMask::LEAVE_WINDOW,
                );
            if cursor != 0 {
                aux = aux.cursor(cursor);
            }
            // InputOnly windows have no depth and no visual (0 = CopyFromParent).
            self.conn.create_window(
                0,
                wid,
                self.root,
                rect.x as i16,
                rect.y as i16,
                rect.w as u16,
                rect.h as u16,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &aux,
            )?;
            self.conn.map_window(wid)?;
            self.conn.configure_window(
                wid,
                &ConfigureWindowAux::new()
                    .stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
            )?;
            ids.push(wid);
        }
        self.conn.flush()?;
        self.bands = ids.clone();
        Ok(ids)
    }

    /// Remove every gap grab area (e.g. a layout with no splittable boundary).
    pub fn grab_bands_hide(&mut self) -> Result<()> {
        for wid in self.bands.drain(..) {
            let _ = self.conn.destroy_window(wid);
        }
        self.conn.flush()?;
        Ok(())
    }

    /// One pending event without blocking, for callers that drain the queue.
    pub fn poll_event(&self) -> Result<Option<Event>> {
        Ok(self.conn.poll_for_event()?)
    }

    /// Ask for the root window's substructure events. A window-manager *layer*
    /// cannot take `SubstructureRedirect` (the desktop's WM owns it), but
    /// `SubstructureNotify` is shared, and EWMH clients send
    /// `_NET_ACTIVE_WINDOW` with both masks — so selecting the notify bit is
    /// enough to receive a pager's or an application's activation request.
    pub fn watch_root(&self) -> Result<()> {
        self.conn.change_window_attributes(
            self.root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// The `_NET_ACTIVE_WINDOW` atom, for matching an incoming request.
    pub fn net_active_window_atom(&self) -> u32 {
        self.atoms.net_active_window
    }

    /// Ask for a managed window's structure events. The WM's synthetic
    /// `ConfigureNotify` (root coordinates, ICCCM) is how the daemon notices
    /// that the user moved a window — including a drag it cannot see.
    pub fn watch_window(&self, win: u32) -> Result<()> {
        self.conn.change_window_attributes(
            win,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
        )?;
        Ok(())
    }

    /// True when a property change on `atom` can change what `describe` shows:
    /// the title or the class. Everything else (size hints, user time, state)
    /// is not worth a redraw.
    pub fn is_title_atom(&self, atom: u32) -> bool {
        atom == self.atoms.net_wm_name
            || atom == self.atoms.wm_class
            || atom == u32::from(AtomEnum::WM_NAME)
    }

    // MARK: hotkeys

    fn lock_combinations(&self) -> Vec<u16> {
        let mut combinations = vec![0];
        for mask in lock_mod_masks(&self.conn) {
            let len = combinations.len();
            for i in 0..len {
                combinations.push(combinations[i] | mask);
            }
        }
        combinations
    }

    fn grab_chord(&self, keycode: u8, mask: u16) -> bool {
        self.conn
            .grab_key(
                false,
                self.root,
                ModMask::from(mask),
                keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map(|cookie| cookie.check().is_ok())
            .unwrap_or(false)
    }

    /// Release every root-window key grab this client owns, so a keyboard-map
    /// change can install the new plan without leaving stale chords behind.
    pub fn ungrab_all_keys(&self) -> Result<()> {
        self.conn
            .ungrab_key(0 /* AnyKey */, self.root, ModMask::ANY)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Grab `bindings` on the root window. Returns the labels that could not be
    /// grabbed (usually because the running WM already owns them).
    pub fn grab_keys(&self, bindings: &[KeyBinding]) -> Result<Vec<String>> {
        let min = self.conn.setup().min_keycode;
        let max = self.conn.setup().max_keycode;
        let count = max - min + 1;
        let mapping = self.conn.get_keyboard_mapping(min, count)?.reply()?;
        let per = mapping.keysyms_per_keycode as usize;
        let mut failed = Vec::new();
        for binding in bindings {
            let mut keycode = None;
            for (i, chunk) in mapping.keysyms.chunks(per).enumerate() {
                if chunk.contains(&binding.keysym) {
                    keycode = Some(min + i as u8);
                    break;
                }
            }
            let Some(keycode) = keycode else {
                failed.push(format!("{} (no keycode)", binding.label));
                continue;
            };
            // BadAccess arrives asynchronously, so check the reply rather than
            // just the send.
            let mut base_ok = false;
            for lock in self.lock_combinations() {
                let ok = self.grab_chord(keycode, binding.mods | lock);
                if lock == 0 {
                    base_ok = ok;
                }
            }
            if !base_ok {
                failed.push(binding.label.clone());
            }
        }
        self.conn.flush()?;
        Ok(failed)
    }

    /// Block for the next event, with a timeout in milliseconds.
    pub fn next_event(&self, timeout_ms: i32) -> Result<Option<Event>> {
        if let Some(event) = self.conn.poll_for_event()? {
            return Ok(Some(event));
        }
        // x11rb has no portable poll timeout; sleep briefly and retry. The
        // daemon's socket thread wakes it through a channel, so this only
        // bounds hotkey latency.
        std::thread::sleep(std::time::Duration::from_millis(timeout_ms.max(1) as u64));
        Ok(self.conn.poll_for_event()?)
    }

    // MARK: helpers

    fn cardinal_prop(&self, win: u32, atom: u32) -> Option<Vec<u32>> {
        let reply = self
            .conn
            .get_property(false, win, atom, AtomEnum::CARDINAL, 0, 64)
            .ok()?
            .reply()
            .ok()?;
        Some(reply.value32().map(|it| it.collect()).unwrap_or_default())
    }

    fn atom_list(&self, win: u32, atom: u32) -> Result<Vec<u32>> {
        let reply = self
            .conn
            .get_property(false, win, atom, AtomEnum::ATOM, 0, 64)?
            .reply()?;
        Ok(reply.value32().map(|it| it.collect()).unwrap_or_default())
    }

    fn string_prop(&self, win: u32, atom: u32) -> Option<String> {
        let reply = self
            .conn
            .get_property(false, win, atom, AtomEnum::ANY, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        if reply.value.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&reply.value).trim_end_matches('\0').to_string())
    }

    /// Set a UTF-8 string property (used for the daemon's status window title).
    pub fn set_utf8(&self, win: u32, atom: u32, value: &str) -> Result<()> {
        self.conn.change_property(
            PropMode::REPLACE,
            win,
            atom,
            self.atoms.utf8_string,
            8,
            value.len() as u32,
            value.as_bytes(),
        )?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn atom(&self, name: &str) -> Result<u32> {
        Ok(self.conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    }
}

impl Atoms {
    fn new(conn: &RustConnection, screen: usize) -> Result<Self> {
        let intern = |name: &str| -> Result<u32> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        let mut type_skip = Vec::new();
        for name in [
            "_NET_WM_WINDOW_TYPE_DOCK",
            "_NET_WM_WINDOW_TYPE_DESKTOP",
            "_NET_WM_WINDOW_TYPE_TOOLBAR",
            "_NET_WM_WINDOW_TYPE_MENU",
            "_NET_WM_WINDOW_TYPE_SPLASH",
            "_NET_WM_WINDOW_TYPE_NOTIFICATION",
            "_NET_WM_WINDOW_TYPE_DROPDOWN_MENU",
            "_NET_WM_WINDOW_TYPE_POPUP_MENU",
            "_NET_WM_WINDOW_TYPE_TOOLTIP",
            "_NET_WM_WINDOW_TYPE_COMBO",
            "_NET_WM_WINDOW_TYPE_DND",
        ] {
            type_skip.push(intern(name)?);
        }
        Ok(Self {
            net_client_list: intern("_NET_CLIENT_LIST")?,
            net_active_window: intern("_NET_ACTIVE_WINDOW")?,
            net_wm_name: intern("_NET_WM_NAME")?,
            net_wm_pid: intern("_NET_WM_PID")?,
            net_wm_window_type: intern("_NET_WM_WINDOW_TYPE")?,
            net_wm_state: intern("_NET_WM_STATE")?,
            net_wm_state_hidden: intern("_NET_WM_STATE_HIDDEN")?,
            net_wm_state_maximized_horz: intern("_NET_WM_STATE_MAXIMIZED_HORZ")?,
            net_wm_state_maximized_vert: intern("_NET_WM_STATE_MAXIMIZED_VERT")?,
            net_wm_state_fullscreen: intern("_NET_WM_STATE_FULLSCREEN")?,
            net_frame_extents: intern("_NET_FRAME_EXTENTS")?,
            net_workarea: intern("_NET_WORKAREA")?,
            net_current_desktop: intern("_NET_CURRENT_DESKTOP")?,
            net_wm_desktop: intern("_NET_WM_DESKTOP")?,
            net_moveresize_window: intern("_NET_MOVERESIZE_WINDOW")?,
            net_wm_cm: intern(&format!("_NET_WM_CM_S{screen}"))?,
            net_wm_window_opacity: intern("_NET_WM_WINDOW_OPACITY")?,
            utf8_string: intern("UTF8_STRING")?,
            wm_class: AtomEnum::WM_CLASS.into(),
            type_skip,
        })
    }
}

/// Pick the work-area quad for `desktop` out of an `_NET_WORKAREA` value, or the
/// whole display when the property is missing, short, or degenerate.
fn pick_workarea(values: Option<Vec<u32>>, desktop: usize, screen: IRect) -> IRect {
    let Some(values) = values else { return screen };
    let Some(quad) = values
        .chunks(4)
        .nth(desktop)
        .or_else(|| values.chunks(4).next())
    else {
        return screen;
    };
    if quad.len() < 4 {
        return screen;
    }
    let rect = IRect::new(quad[0] as i32, quad[1] as i32, quad[2] as i32, quad[3] as i32);
    if rect.w <= 0 || rect.h <= 0 {
        return screen;
    }
    rect
}

/// Modifier masks used by the default bindings.
pub mod mods {
    // X modifier bits: Shift = 1, Lock = 2, Control = 4, Mod1 = 8, Mod4 = 64.
    pub const CTRL_ALT: u16 = 4 | 8;
    pub const CTRL_ALT_SHIFT: u16 = 4 | 8 | 1;
    /// Every modifier a binding may name. Lock (Caps Lock) and the other
    /// modifiers are deliberately absent: they state a keyboard state, not a
    /// chord.
    pub const BINDING: u16 = 1 | 4 | 8 | 64;
}

/// The modifier bits a key event is matched on.
///
/// X delivers the raw modifier state, which includes Caps Lock (`Lock`) and,
/// on most maps, Num Lock (`Mod2`). Matching the raw state means every binding
/// silently dies the moment either lock is on — the single most-reported
/// hotkey bug in bspwm (#16), xmonad (#1), spectrwm (#103) and i3 (#943).
/// Masking to the modifiers a binding can actually name fixes that class.
pub fn event_mods(state: u16) -> u16 {
    state & mods::BINDING
}

/// X keysyms for the default bindings (values from `<X11/keysymdef.h>`).
pub mod keysym {
    pub const SPACE: u32 = 0x0020;
    pub const RETURN: u32 = 0xff0d;
    pub const UP: u32 = 0xff52;
    pub const DOWN: u32 = 0xff54;
    pub const LEFT: u32 = 0xff51;
    pub const RIGHT: u32 = 0xff53;
    pub const BRACKET_LEFT: u32 = 0x005b;
    pub const BRACKET_RIGHT: u32 = 0x005d;
    pub const D: u32 = 0x0064;
    pub const N: u32 = 0x006e;
    pub const A: u32 = 0x0061;
    pub const F: u32 = 0x0066;
    pub const O: u32 = 0x006f;
    pub const DIGIT_1: u32 = 0x0031;
    pub const DIGIT_9: u32 = 0x0039;
}

/// The key map: each command has a primary binding (matching the TrellisWM
/// guide's `⌃⌥` shortcuts) and a `Super` fallback for desktops that already own
/// the `Ctrl+Alt` combination — xfwm4 does, for workspace switching.
/// Cursors for a draggable boundary: the standard `cursor` font's
/// `sb_h_double_arrow` (108) and `sb_v_double_arrow` (116), each followed by
/// its mask glyph. Falls back to `None` on a server without core fonts.
fn make_resize_cursors(conn: &RustConnection) -> Result<(Cursor, Cursor)> {
    let font = conn.generate_id()?;
    conn.open_font(font, b"cursor")?;
    let glyph = |conn: &RustConnection, source: u16| -> Result<Cursor> {
        let id = conn.generate_id()?;
        // fg white, bg black: legible on light and dark tiles.
        conn.create_glyph_cursor(id, font, font, source, source + 1, 65535, 65535, 65535, 0, 0, 0)?;
        Ok(id)
    };
    let h = glyph(conn, 108)?;
    let v = glyph(conn, 116)?;
    conn.close_font(font)?;
    conn.flush()?;
    Ok((h, v))
}

/// The default bindings with a user's per-command overrides applied. A command
/// named in `overrides` replaces its default candidates entirely (an empty list
/// unbinds it); commands not mentioned keep the defaults, and an override for a
/// command outside the default plan is appended, so any command the daemon
/// understands (`view 3`, `gap 12`, `frame-desktop`, …) can be put on a chord.
///
/// `tags` adds the named-workspace chords (`tag-view 1`…`9`, `window-tag
/// 1`…`9`). It is a parameter, not a config read, because the default plan is
/// also what `keys --check` and the settings rows display, and a desktop that
/// has not opted into workspaces must not see nine chords it never asked for.
pub fn binding_plan_with(
    overrides: &std::collections::HashMap<String, Vec<String>>,
    tags: bool,
) -> Vec<(String, Vec<String>)> {
    let mut base: Vec<(String, Vec<String>)> = binding_plan()
        .into_iter()
        .map(|(command, defaults)| {
            (
                command.to_string(),
                defaults.into_iter().map(str::to_string).collect(),
            )
        })
        .collect();
    if tags {
        base.extend(tag_bindings());
    }
    let mut plan: Vec<(String, Vec<String>)> = base
        .into_iter()
        .map(|(command, defaults)| {
            let specs = match overrides.get(&command) {
                Some(user) => user.clone(),
                None => defaults,
            };
            (command, specs)
        })
        .collect();
    // Extra commands the user invented: append in a stable order so
    // `pergola keys --check` is deterministic.
    let mut extra: Vec<(&String, &Vec<String>)> = overrides
        .iter()
        .filter(|(command, _)| !plan.iter().any(|(known, _)| known == *command))
        .collect();
    extra.sort_by(|a, b| a.0.cmp(b.0));
    plan.extend(extra.into_iter().map(|(c, s)| (c.clone(), s.clone())));
    plan
}

/// The named-workspace chords, appended only when the `tags` config key is on.
/// `Ctrl+Alt+<n>` and `Super+<n>` switch to a tag; adding `Shift` moves the
/// focused window onto it. `Super+1`…`9` is the convention every tiling desktop
/// shares, so a person who turns workspaces on already knows the chord.
fn tag_bindings() -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for n in 1..=9 {
        out.push((
            format!("tag-view {n}"),
            vec![format!("Ctrl+Alt+{n}"), format!("Super+{n}")],
        ));
        out.push((
            format!("window-tag {n}"),
            vec![
                format!("Ctrl+Alt+Shift+{n}"),
                format!("Super+Shift+{n}"),
            ],
        ));
    }
    out
}

pub fn binding_plan() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("everything", vec!["Ctrl+Alt+Space", "Super+Space"]),
        ("zoom-out", vec!["Ctrl+Alt+Up", "Super+Up"]),
        ("zoom-in", vec!["Ctrl+Alt+Down", "Super+Down"]),
        ("neighbour?dir=left", vec!["Ctrl+Alt+Left", "Super+Left"]),
        ("neighbour?dir=right", vec!["Ctrl+Alt+Right", "Super+Right"]),
        ("back", vec!["Ctrl+Alt+[", "Super+["]),
        ("forward", vec!["Ctrl+Alt+]", "Super+]"]),
        ("frame-window", vec!["Ctrl+Alt+Return", "Super+Return"]),
        ("toggle-desktop", vec!["Ctrl+Alt+d", "Super+d"]),
        ("split-terminal", vec!["Ctrl+Alt+n", "Super+n"]),
        ("arrange", vec!["Ctrl+Alt+a", "Super+a"]),
        ("float-all", vec!["Ctrl+Alt+f", "Super+f"]),
        ("overlay", vec!["Ctrl+Alt+o", "Super+o"]),
        // A layout change can be undone; the camera's back/forward is not that.
        ("undo", vec!["Ctrl+Alt+z", "Super+z"]),
        // Nudge the split around the focused panel without a pointer. The value
        // is a delta, so the same chord works whatever the split's size is.
        ("ratio -0.05", vec!["Ctrl+Alt+comma", "Super+comma"]),
        ("ratio +0.05", vec!["Ctrl+Alt+period", "Super+period"]),
        // Mouse-free tiling: the four edges a window can be docked against.
        ("dock left", vec!["Ctrl+Alt+Shift+Left", "Super+Shift+Left"]),
        ("dock right", vec!["Ctrl+Alt+Shift+Right", "Super+Shift+Right"]),
        ("dock up", vec!["Ctrl+Alt+Shift+Up", "Super+Shift+Up"]),
        ("dock down", vec!["Ctrl+Alt+Shift+Down", "Super+Shift+Down"]),
        ("float", vec!["Ctrl+Alt+Shift+w", "Super+Shift+w"]),
        // Discoverability: a menu of the same actions, for the mouse.
        ("palette", vec!["Ctrl+Alt+m", "Super+m"]),
        // Tab groups: cycle the shown view of the focused group.
        ("tab-next", vec!["Ctrl+Alt+Page_Down", "Super+Page_Down"]),
        ("tab-prev", vec!["Ctrl+Alt+Page_Up", "Super+Page_Up"]),
    ]
}

/// A plan entry that could not be bound on any candidate key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindFailure {
    pub command: String,
    pub tried: Vec<String>,
}

impl X11 {
    /// Bind the first free candidate for every command. Returns the bindings
    /// that were installed and the ones that had to be skipped.
    pub fn install_bindings(
        &self,
        plan: &[(String, Vec<String>)],
    ) -> Result<(Vec<(KeyBinding, String)>, Vec<BindFailure>)> {
        self.refresh_bindings(plan, &mut BTreeSet::new())
    }

    /// Keep unchanged passive grabs live while reconciling the server keymap.
    pub fn refresh_bindings(
        &self,
        plan: &[(String, Vec<String>)],
        held: &mut BTreeSet<(u8, u16)>,
    ) -> Result<(Vec<(KeyBinding, String)>, Vec<BindFailure>)> {
        let min = self.conn.setup().min_keycode;
        let max = self.conn.setup().max_keycode;
        let mapping = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)?
            .reply()?;
        let per = mapping.keysyms_per_keycode as usize;
        let keycode_for = |sym: u32| -> Option<u8> {
            mapping
                .keysyms
                .chunks(per)
                .position(|chunk| chunk.contains(&sym))
                .map(|i| min + i as u8)
        };
        let locks = self.lock_combinations();
        let mut desired = BTreeSet::new();
        let mut installed = Vec::new();
        let mut failures = Vec::new();
        for (command, specs) in plan {
            // An empty candidate list is an intentional unbind, not a failure.
            if specs.is_empty() {
                continue;
            }
            let mut bound = false;
            for spec in specs {
                let Some(binding) = parse_binding(spec) else {
                    continue;
                };
                let Some(keycode) = keycode_for(binding.keysym) else {
                    continue;
                };
                let base = (keycode, binding.mods);
                if !held.contains(&base) && !self.grab_chord(keycode, binding.mods) {
                    continue;
                }
                desired.insert(base);
                for lock in &locks {
                    let chord = (keycode, binding.mods | lock);
                    if desired.contains(&chord)
                        || held.contains(&chord)
                        || self.grab_chord(chord.0, chord.1)
                    {
                        desired.insert(chord);
                    }
                }
                installed.push((binding, command.clone()));
                bound = true;
                break;
            }
            if !bound {
                failures.push(BindFailure {
                    command: command.clone(),
                    tried: specs.iter().map(|s| s.to_string()).collect(),
                });
            }
        }
        for &(keycode, mask) in held.difference(&desired) {
            self.conn
                .ungrab_key(keycode, self.root, ModMask::from(mask))?;
        }
        *held = desired;
        self.conn.flush()?;
        Ok((installed, failures))
    }
}

/// Look up a keysym by its X name (lowercase letters, digits, and the common
/// named keys are enough for binding specs).
pub fn keysym_from_name(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "space" => Some(keysym::SPACE),
        "return" | "enter" => Some(keysym::RETURN),
        "left" => Some(keysym::LEFT),
        "right" => Some(keysym::RIGHT),
        "up" => Some(keysym::UP),
        "down" => Some(keysym::DOWN),
        "[" | "bracketleft" => Some(keysym::BRACKET_LEFT),
        "]" | "bracketright" => Some(keysym::BRACKET_RIGHT),
        "tab" => Some(0xff09),
        "escape" | "esc" => Some(0xff1b),
        "home" => Some(0xff50),
        "end" => Some(0xff57),
        "page_up" | "prior" | "pageup" => Some(0xff55),
        "page_down" | "next" | "pagedown" => Some(0xff56),
        "comma" => Some(0x002c),
        "period" => Some(0x002e),
        "slash" => Some(0x002f),
        "semicolon" => Some(0x003b),
        "apostrophe" => Some(0x0027),
        "minus" => Some(0x002d),
        "equal" => Some(0x003d),
        "backslash" => Some(0x005c),
        "grave" => Some(0x0060),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    let mut chars = lower.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    None
}

/// Parse `Ctrl+Alt+Space` or `Super+Left` into a binding.
pub fn parse_binding(spec: &str) -> Option<KeyBinding> {
    let mut mods = 0u16;
    let mut key = None;
    for part in spec.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= 4,
            "alt" | "mod1" | "meta" => mods |= 8,
            "shift" => mods |= 1,
            "super" | "mod4" | "win" => mods |= 64,
            _ => {
                if key.is_some() {
                    return None;
                }
                key = keysym_from_name(part);
            }
        }
    }
    Some(KeyBinding {
        mods,
        keysym: key?,
        label: spec.to_string(),
    })
}

/// Map a key event back to a command string.
pub fn command_for_key<'a>(
    bindings: &'a [(KeyBinding, String)],
    mods: u16,
    keysym: u32,
) -> Option<&'a str> {
    bindings
        .iter()
        .find(|(b, _)| b.mods == mods && b.keysym == keysym)
        .map(|(_, cmd)| cmd.as_str())
}

/// Convert a core `Rect` to an `IRect`.
pub fn to_irect(r: Rect) -> IRect {
    r.to_irect()
}

/// Modifier masks to grab *in addition to* a binding's own mask, so the grab
/// still fires with Caps Lock or Num Lock on. Computed from the modifier map:
/// the mask that carries `Num_Lock` is platform-dependent (usually Mod2, but
/// some layouts put it on Mod3 or Mod5), and guessing it is how "bindings stop
/// working with numlock" happens.
pub fn lock_mod_masks(conn: &RustConnection) -> Vec<u16> {
    use x11rb::protocol::xproto::ConnectionExt as _;
    let mut masks = vec![1u16 << 1]; // Lock (Caps Lock), fixed by the core protocol.
    let setup = conn.setup();
    let min = setup.min_keycode;
    let max = setup.max_keycode;
    let count = max - min + 1;
    let Some(binding) = conn
        .get_keyboard_mapping(min, count)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return masks;
    };
    let per_key = binding.keysyms_per_keycode as usize;
    let keycode_sym = |code: u8| -> Option<u32> {
        let index = (code.saturating_sub(min)) as usize * per_key;
        binding.keysyms.get(index).copied()
    };
    let Some(modmap) = conn.get_modifier_mapping().ok().and_then(|c| c.reply().ok()) else {
        return masks;
    };
    let per_mod = (modmap.keycodes.len() / 8).max(1);
    for (index, row) in modmap.keycodes.chunks(per_mod).enumerate() {
        if index >= 8 {
            break;
        }
        // Num_Lock = 0xff7f, Caps_Lock = 0xffe5.
        if row
            .iter()
            .any(|&code| matches!(keycode_sym(code), Some(0xff7f) | Some(0xffe5)))
        {
            masks.push(1u16 << index);
        }
    }
    masks.sort_unstable();
    masks.dedup();
    masks
}

/// Human-readable summary of a window, for `pergola list`.
pub fn describe(info: &WindowInfo) -> String {
    format!(
        "0x{:08x}  {:>4}x{:<4} +{:<5}+{:<5}  {:<20}  {}",
        info.id,
        info.rect.w,
        info.rect.h,
        info.rect.x,
        info.rect.y,
        truncate(&info.class, 20),
        truncate(&info.title, 60)
    )
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Check that the display is reachable and EWMH is present.
pub fn doctor() -> Result<Vec<String>> {
    let x = X11::connect()?;
    let mut notes = Vec::new();
    notes.push(format!("display: {}", std::env::var("DISPLAY").unwrap_or_default()));
    let screen = x.screen_rect()?;
    notes.push(format!("screen: {}x{}", screen.w, screen.h));
    let clients = x.client_list()?;
    notes.push(format!("EWMH client list: {} windows", clients.len()));
    let wm = x
        .conn
        .get_property(
            false,
            x.root,
            x.atom("_NET_SUPPORTING_WM_CHECK")?,
            AtomEnum::WINDOW,
            0,
            1,
        )?
        .reply()?;
    let wm_name = wm
        .value32()
        .and_then(|mut it| it.next())
        .and_then(|w| x.string_prop(w, x.atom("_NET_WM_NAME").ok()?))
        .unwrap_or_else(|| "unknown".to_string());
    notes.push(format!("running WM: {wm_name}"));
    // The pointer hints need SHAPE for their click-through input region; say so
    // here rather than letting a user wonder why hovering a gap draws nothing.
    notes.push(format!(
        "SHAPE extension: {}",
        if x.shape_available() { "yes" } else { "no (pointer hints disabled)" }
    ));
    if clients.is_empty() {
        return Err(anyhow!(
            "no EWMH client list — this desktop does not advertise _NET_CLIENT_LIST"
        ));
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: IRect = IRect { x: 0, y: 0, w: 1600, h: 900 };

    #[test]
    fn workarea_uses_the_current_desktop_quad() {
        // Two desktops: only the second has a top panel.
        let values = vec![0, 0, 1600, 900, 0, 27, 1600, 873];
        assert_eq!(
            pick_workarea(Some(values.clone()), 1, SCREEN),
            IRect::new(0, 27, 1600, 873)
        );
        assert_eq!(pick_workarea(Some(values), 0, SCREEN), SCREEN);
    }

    #[test]
    fn workarea_falls_back_to_the_display() {
        assert_eq!(pick_workarea(None, 0, SCREEN), SCREEN);
        assert_eq!(pick_workarea(Some(vec![]), 0, SCREEN), SCREEN);
        assert_eq!(pick_workarea(Some(vec![1, 2, 0, 5]), 0, SCREEN), SCREEN);
        // Out-of-range desktop falls back to the first quad, not a panic.
        assert_eq!(
            pick_workarea(Some(vec![0, 27, 1600, 873]), 4, SCREEN),
            IRect::new(0, 27, 1600, 873)
        );
    }

    #[test]
    fn event_mods_ignores_caps_and_num_lock_but_keeps_super() {
        const SHIFT: u16 = 1;
        const LOCK: u16 = 2;
        const CONTROL: u16 = 4;
        const MOD1: u16 = 8;
        const MOD2: u16 = 16; // Num Lock on the default map
        const MOD4: u16 = 64;
        // Ctrl+Alt+Space with Caps Lock and Num Lock both on.
        assert_eq!(
            event_mods(CONTROL | MOD1 | LOCK | MOD2),
            CONTROL | MOD1
        );
        // A Super chord must keep Mod4; forgetting it is how the documented
        // fallback silently stops working on desktops that own Ctrl+Alt.
        assert_eq!(event_mods(MOD4 | LOCK | MOD2), MOD4);
        assert_eq!(event_mods(SHIFT | CONTROL | MOD1 | MOD4), SHIFT | CONTROL | MOD1 | MOD4);
    }

    #[test]
    fn client_size_snaps_down_to_the_increment_grid() {
        // A terminal with a 6x13 grid and no base size.
        assert_eq!(snap_client_size(900, 0, 6, 0, 0), 900);
        assert_eq!(snap_client_size(901, 0, 6, 0, 0), 900);
        assert_eq!(snap_client_size(895, 0, 6, 0, 0), 894);
        // Base size is the origin of the grid: (900 - 4) / 6 = 149 steps.
        assert_eq!(snap_client_size(900, 4, 6, 0, 0), 4 + 149 * 6);
        // No increments: the available size stands.
        assert_eq!(snap_client_size(901, 0, 0, 0, 0), 901);
        // Minimum wins over the tile, maximum caps a too-large tile.
        assert_eq!(snap_client_size(200, 0, 10, 250, 0), 250);
        assert_eq!(snap_client_size(900, 0, 10, 0, 400), 400);
    }

    #[test]
    fn snap_to_hints_keeps_the_frame_origin_and_the_decoration() {
        let frame = IRect::new(100, 50, 906, 313);
        let hints = SizeHints {
            inc_w: 6,
            inc_h: 13,
            ..SizeHints::default()
        };
        // 4 px of left/right decoration, 2 px of top/bottom.
        let out = snap_to_hints(frame, hints, (2, 2, 2, 2));
        assert_eq!(out.x, 100);
        assert_eq!(out.y, 50);
        assert_eq!(out.w - 4, 900); // 902 client -> 6 * 150 = 900
        assert_eq!(out.h - 4, 299); // 309 client -> 13 * 23 = 299
    }

    #[test]
    fn snap_to_hints_is_identity_without_increments() {
        let frame = IRect::new(0, 0, 101, 77);
        assert_eq!(snap_to_hints(frame, SizeHints::default(), (0, 0, 0, 0)), frame);
    }

    #[test]
    fn binding_overrides_replace_only_the_named_command() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("zoom-out".to_string(), vec!["Super+Page_Up".to_string()]);
        let plan = binding_plan_with(&overrides, false);
        let find = |command: &str| {
            plan.iter()
                .find(|(c, _)| c == command)
                .map(|(_, specs)| specs.clone())
                .unwrap_or_else(|| panic!("missing {command}"))
        };
        // The named command loses its defaults entirely.
        assert_eq!(find("zoom-out"), vec!["Super+Page_Up".to_string()]);
        // An unnamed command keeps its default candidates.
        assert_eq!(
            find("zoom-in"),
            vec!["Ctrl+Alt+Down".to_string(), "Super+Down".to_string()]
        );
        // Nothing else is lost: the default plan's length is preserved.
        assert_eq!(plan.len(), binding_plan().len());
    }

    #[test]
    fn binding_overrides_can_add_and_unbind_commands() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("view 3".to_string(), vec!["Super+3".to_string()]);
        overrides.insert("overlay".to_string(), Vec::new());
        let plan = binding_plan_with(&overrides, false);
        // An invented command is appended, in a deterministic position.
        let last = plan.last().expect("plan is not empty");
        assert_eq!(last.0, "view 3");
        assert_eq!(last.1, vec!["Super+3".to_string()]);
        // An empty list is kept as an intentional unbind, not dropped.
        let overlay = plan.iter().find(|(c, _)| c == "overlay").expect("overlay");
        assert!(overlay.1.is_empty());
    }
}
