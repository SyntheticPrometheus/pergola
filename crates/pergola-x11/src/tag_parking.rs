use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, PropMode,
};
use x11rb::wrapper::ConnectionExt as _;

use crate::X11;

const NORMAL: u32 = 1;
const ICONIC: u32 = 3;

// The marker lives on the client and disappears when that XID is destroyed.
// Its phase survives daemon restart and orders a rapid hide/show pair.
const HIDING: u32 = 1;
const HIDDEN: u32 = 2;
const SHOWING: u32 = 3;

pub struct TagParking {
    pub native: bool,
    mark: u32,
    wm_state: u32,
    change_state: u32,
    current_desktop: u32,
    owned: HashMap<u32, u32>,
}

impl TagParking {
    pub fn new(x: &X11, layout: &Path, windows: impl Iterator<Item = u32>) -> Result<Self> {
        let wm_check = x.atom("_NET_SUPPORTING_WM_CHECK")?;
        let manager = x
            .conn
            .get_property(false, x.root, wm_check, AtomEnum::WINDOW, 0, 1)?
            .reply()?
            .value32()
            .and_then(|mut v| v.next());
        let native = manager
            .and_then(|w| x.string_prop(w, x.atoms.net_wm_name))
            .is_some_and(|name| name.eq_ignore_ascii_case("Openbox"));
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        layout.hash(&mut hash);
        let mark = x.atom(&format!("_PERGOLA_TAG_PARKED_{:016X}", hash.finish()))?;
        let mut owned = HashMap::new();
        for win in windows {
            if let Some(phase) = x.cardinal_prop(win, mark).and_then(|v| v.first().copied()) {
                if matches!(phase, HIDING | HIDDEN | SHOWING) {
                    owned.insert(win, phase);
                }
            }
        }
        Ok(Self {
            native,
            mark,
            wm_state: x.atom("WM_STATE")?,
            change_state: x.atom("WM_CHANGE_STATE")?,
            current_desktop: x.atoms.net_current_desktop,
            owned,
        })
    }

    pub fn has_owned(&self) -> bool {
        !self.owned.is_empty()
    }
    pub fn owns(&self, win: u32) -> bool {
        self.owned.contains_key(&win)
    }
    pub fn forget(&mut self, win: u32) {
        self.owned.remove(&win);
    }
    pub fn observes(&self, win: u32, atom: u32) -> bool {
        self.owns(win) && atom == self.wm_state
    }
    pub fn host_switch(&self, atom: u32) -> bool {
        atom == self.current_desktop
    }

    fn phase(&mut self, x: &X11, win: u32, phase: u32) -> Result<()> {
        if phase == 0 {
            x.conn.delete_property(win, self.mark)?;
            self.owned.remove(&win);
        } else {
            x.conn.change_property32(
                PropMode::REPLACE,
                win,
                self.mark,
                AtomEnum::CARDINAL,
                &[phase],
            )?;
            self.owned.insert(win, phase);
        }
        x.conn.flush()?;
        Ok(())
    }

    /// Returns true when the client has actually finished remapping.
    pub fn sync(&mut self, x: &X11, win: u32, hide: bool) -> Result<bool> {
        let phase = self.owned.get(&win).copied();
        if !self.native && phase.is_none() || !hide && phase.is_none() {
            return Ok(false);
        }
        let state = x
            .conn
            .get_property(false, win, self.wm_state, self.wm_state, 0, 1)?
            .reply()?
            .value32()
            .and_then(|mut v| v.next());
        match (hide && self.native, phase, state) {
            (true, None, Some(NORMAL)) | (true, Some(SHOWING | HIDDEN), Some(NORMAL)) => {
                let Some(s) = x.info(win)? else {
                    return Ok(false);
                };
                if s.hidden || s.transient || s.fullscreen {
                    return Ok(false);
                }
                self.phase(x, win, HIDING)?;
                let event =
                    ClientMessageEvent::new(32, win, self.change_state, [ICONIC, 0, 0, 0, 0]);
                x.conn.send_event(
                    false,
                    x.root,
                    EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                    event,
                )?;
                x.conn.flush()?;
            }
            (true, Some(HIDING), Some(ICONIC)) => self.phase(x, win, HIDDEN)?,
            (false, Some(HIDDEN | HIDING), Some(ICONIC)) => {
                self.phase(x, win, SHOWING)?;
                x.conn.map_window(win)?;
                x.conn.flush()?;
            }
            (false, Some(SHOWING | HIDDEN), Some(NORMAL)) => {
                self.phase(x, win, 0)?;
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }
}
