use anyhow::{Context, Result, bail};
use gtk::{gdk, glib::translate::IntoGlib};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use x11rb::{
    connection::Connection,
    protocol::{
        Event, xkb,
        xproto::{ConnectionExt, GrabMode, ModMask},
    },
    rust_connection::RustConnection,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Bindings {
    pub toggle: String,
    pub push_to_talk: String,
    pub start: String,
    pub stop: String,
}
impl Default for Bindings {
    fn default() -> Self {
        Self {
            toggle: "F9".into(),
            push_to_talk: "F8".into(),
            start: String::new(),
            stop: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Toggle,
    Start,
    Stop,
    PttStart,
    PttStop,
}

pub struct Hotkeys {
    connection: RustConnection,
    grabs: BTreeSet<(u32, u8, u16)>,
    bindings: HashMap<(u8, u16), Action>,
    pressed: HashMap<u8, Action>,
    ignored_modifiers: u16,
}

fn parse(text: &str) -> Result<Option<(u32, u16)>> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let parts: Vec<_> = text.trim().split('+').map(str::trim).collect();
    let mut modifiers = 0;
    for part in &parts[..parts.len() - 1] {
        modifiers |= match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => 4,
            "shift" => 1,
            "alt" => 8,
            "super" | "win" | "mod4" => 64,
            _ => bail!("Unknown modifier '{part}'. Use Ctrl, Shift, Alt, or Super."),
        };
    }
    let name = parts.last().unwrap();
    let name = if name.len() == 1 {
        name.to_ascii_lowercase()
    } else if name.to_ascii_lowercase().starts_with('f') && name[1..].parse::<u8>().is_ok() {
        name.to_ascii_uppercase()
    } else if name.eq_ignore_ascii_case("space") {
        "space".into()
    } else {
        (*name).to_owned()
    };
    let key =
        gdk::Key::from_name(&name).context("Unknown key. Try F8, F9, or Ctrl+Shift+space.")?;
    let symbol = key.into_glib();
    if matches!(symbol, 0xffe1..=0xffee | 0xff7f | 0xff14) {
        bail!("Choose a regular key, optionally with modifiers.");
    }
    Ok(Some((symbol, modifiers)))
}

impl Hotkeys {
    pub fn new() -> Result<Self> {
        let (connection, _) =
            x11rb::connect(None).context("Global shortcuts need an X11 display")?;
        if !xkb::use_extension(&connection, 1, 0)?.reply()?.supported {
            bail!("XKB is required for reliable push-to-talk.");
        }
        let flag = xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT;
        let reply = xkb::per_client_flags(
            &connection,
            xkb::ID::USE_CORE_KBD.into(),
            flag,
            flag,
            0u32.into(),
            0u32.into(),
            0u32.into(),
        )?
        .reply()?;
        if u32::from(reply.value) & u32::from(flag) == 0 {
            bail!("This X11 server cannot distinguish key repeats from releases.");
        }
        Ok(Self {
            connection,
            grabs: BTreeSet::new(),
            bindings: HashMap::new(),
            pressed: HashMap::new(),
            ignored_modifiers: 0,
        })
    }

    /// Grab new bindings before releasing old ones, rolling back on conflict.
    pub fn configure(&mut self, settings: &Bindings) -> Result<()> {
        if !self.pressed.is_empty() {
            bail!("Release the recording shortcut before changing bindings.");
        }
        let setup = self.connection.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - first + 1;
        let keyboard = self
            .connection
            .get_keyboard_mapping(first, count)?
            .reply()?;
        let symbols_per_key = keyboard.keysyms_per_keycode as usize;
        let symbols = |code: u8| -> &[u32] {
            if code < first || code > setup.max_keycode {
                return &[];
            }
            let offset = (code - first) as usize * symbols_per_key;
            &keyboard.keysyms[offset..offset + symbols_per_key]
        };
        let modifiers = self.connection.get_modifier_mapping()?.reply()?;
        let mut ignored = 2u16; // Caps Lock, plus wherever Num/Scroll Lock are mapped.
        let per_modifier = modifiers.keycodes.len() / 8;
        for (index, codes) in modifiers.keycodes.chunks(per_modifier.max(1)).enumerate() {
            if codes
                .iter()
                .any(|&code| symbols(code).iter().any(|s| matches!(*s, 0xff7f | 0xff14)))
            {
                ignored |= 1 << index;
            }
        }
        let mut bindings = HashMap::new();
        let mut grabs = BTreeSet::new();
        for (text, action) in [
            (&settings.toggle, Action::Toggle),
            (&settings.push_to_talk, Action::PttStart),
            (&settings.start, Action::Start),
            (&settings.stop, Action::Stop),
        ] {
            let Some((symbol, mask)) = parse(text)? else {
                continue;
            };
            let code = (first..=setup.max_keycode)
                .find(|&code| symbols(code).contains(&symbol))
                .with_context(|| {
                    format!("'{text}' is not present in the current keyboard layout")
                })?;
            if bindings.insert((code, mask), action).is_some() {
                bail!("Each action needs a different shortcut: {text}");
            }
            for root in &setup.roots {
                for locks in 0u16..256 {
                    if locks & !ignored == 0 {
                        grabs.insert((root.root, code, mask | locks));
                    }
                }
            }
        }
        let additions: Vec<_> = grabs.difference(&self.grabs).copied().collect();
        let mut added = Vec::new();
        for &(root, code, mask) in &additions {
            let result = self
                .connection
                .grab_key(
                    false,
                    root,
                    ModMask::from(mask),
                    code,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?
                .check();
            if result.is_err() {
                for &(root, code, mask) in &added {
                    self.connection
                        .ungrab_key(code, root, ModMask::from(mask))?
                        .check()?;
                }
                bail!(
                    "A shortcut is already in use by i3 or another application. Choose another key or remove its other binding."
                );
            }
            added.push((root, code, mask));
        }
        for &(root, code, mask) in self.grabs.difference(&grabs) {
            self.connection
                .ungrab_key(code, root, ModMask::from(mask))?
                .check()?;
        }
        self.connection.flush()?;
        self.grabs = grabs;
        self.bindings = bindings;
        self.ignored_modifiers = ignored;
        Ok(())
    }

    pub fn is_pressed(&self) -> bool {
        !self.pressed.is_empty()
    }

    pub fn poll(&mut self) -> Result<Vec<Action>> {
        let mut actions = Vec::new();
        while let Some(event) = self.connection.poll_for_event()? {
            match event {
                Event::KeyPress(event) => {
                    if self.pressed.contains_key(&event.detail) {
                        continue;
                    }
                    let mask = u16::from(event.state) & 0xff & !self.ignored_modifiers;
                    if let Some(&action) = self.bindings.get(&(event.detail, mask)) {
                        self.pressed.insert(event.detail, action);
                        actions.push(action);
                    }
                }
                Event::KeyRelease(event)
                    if self.pressed.remove(&event.detail) == Some(Action::PttStart) =>
                {
                    actions.push(Action::PttStop);
                }
                _ => {}
            }
        }
        Ok(actions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortcut_parser_accepts_optional_bindings_and_rejects_typos() {
        assert_eq!(parse("").unwrap(), None);
        assert_eq!(parse("Ctrl+Shift+space").unwrap(), Some((32, 5)));
        assert_eq!(parse("f9").unwrap(), parse("F9").unwrap());
        assert!(parse("Hyper+F9").is_err());
        assert!(parse("Control_L").is_err());
        assert!(parse("NoSuchKey").is_err());
    }
    #[test]
    #[ignore = "Uses the live X11 display and temporarily grabs Ctrl+Shift+F6/F7"]
    fn native_grabs_repeat_release_and_conflict_rollback() {
        use std::{process::Command, thread, time::Duration};
        let settings = Bindings {
            toggle: "Ctrl+Shift+F6".into(),
            push_to_talk: "Ctrl+Shift+F7".into(),
            start: String::new(),
            stop: String::new(),
        };
        let mut keys = Hotkeys::new().unwrap();
        keys.configure(&settings).unwrap();
        let mut other = Hotkeys::new().unwrap();
        assert!(other.configure(&settings).is_err());
        // Failed reconfiguration must leave the original keys working.
        assert!(
            keys.configure(&Bindings {
                start: settings.toggle.clone(),
                ..settings.clone()
            })
            .is_err()
        );
        struct Release;
        impl Drop for Release {
            fn drop(&mut self) {
                let _ = Command::new("xdotool")
                    .args(["keyup", "F6", "F7", "Control_L", "Shift_L"])
                    .status();
            }
        }
        let _release = Release;
        assert!(
            Command::new("xdotool")
                .args(["keydown", "Control_L", "Shift_L", "F7", "F7"])
                .status()
                .unwrap()
                .success()
        );
        thread::sleep(Duration::from_millis(40));
        assert_eq!(keys.poll().unwrap(), vec![Action::PttStart]);
        // Releasing modifiers first must still stop on the main key's release.
        Command::new("xdotool")
            .args(["keyup", "Control_L", "Shift_L", "F7"])
            .status()
            .unwrap();
        thread::sleep(Duration::from_millis(40));
        assert_eq!(keys.poll().unwrap(), vec![Action::PttStop]);
        Command::new("xdotool")
            .args(["key", "ctrl+shift+F6"])
            .status()
            .unwrap();
        thread::sleep(Duration::from_millis(40));
        assert_eq!(keys.poll().unwrap(), vec![Action::Toggle]);
        keys.configure(&Bindings {
            toggle: String::new(),
            push_to_talk: String::new(),
            start: String::new(),
            stop: String::new(),
        })
        .unwrap();
        other.configure(&settings).unwrap();
    }
}
