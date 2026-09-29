use anyhow::{Context, Result, bail};
use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

pub const SHORTCUTS: [&str; 4] = ["auto", "ctrl+v", "ctrl+shift+v", "shift+Insert"];

fn shortcut_for_class(class: &[u8]) -> &'static str {
    // Match either WM_CLASS component: the user's scratch terminal overrides
    // the class, but retains the instance name "ghostty".
    if class.split(|&b| b == 0).any(|part| {
        part.eq_ignore_ascii_case(b"ghostty") || part.eq_ignore_ascii_case(b"com.mitchellh.ghostty")
    }) {
        "ctrl+shift+v"
    } else {
        "ctrl+v"
    }
}

fn automatic_shortcut() -> Result<&'static str> {
    let (connection, _) =
        x11rb::connect(None).context("Cannot identify the focused X11 application")?;
    let focus = connection.get_input_focus()?.reply()?.focus;
    let mut window = focus;
    let mut shortcut = "ctrl+v";
    // Input focus can be a child of the window carrying WM_CLASS.
    for _ in 0..32 {
        if window <= 1 {
            break;
        }
        let class = connection
            .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)?
            .reply()?;
        if !class.value.is_empty() {
            shortcut = shortcut_for_class(&class.value);
            break;
        }
        let tree = connection.query_tree(window)?.reply()?;
        if window == tree.root || tree.parent == window {
            break;
        }
        window = tree.parent;
    }
    if connection.get_input_focus()?.reply()?.focus != focus {
        bail!("Focus changed before auto-paste. Transcript copied; paste manually.");
    }
    Ok(shortcut)
}

// Run off the GTK thread: the target may immediately request clipboard contents.
// No Enter/Return is sent; this only invokes the target application's Paste action.
pub fn paste(shortcut: &str) -> Result<()> {
    if !SHORTCUTS.contains(&shortcut) {
        bail!("Unsupported paste shortcut");
    }
    let shortcut = if shortcut == "auto" {
        automatic_shortcut()?
    } else {
        shortcut
    };
    let mut child = Command::new("xdotool")
        .args(["key", "--clearmodifiers", "--delay", "20", shortcut])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Auto-paste needs xdotool; the transcript is still on the clipboard")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("Could not send the paste shortcut. Transcript copied; paste manually.");
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Auto-paste timed out. Transcript copied; paste manually.");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_mode_recognizes_ghostty_with_custom_window_classes() {
        assert_eq!(
            shortcut_for_class(b"ghostty\0com.mitchellh.ghostty\0"),
            "ctrl+shift+v"
        );
        assert_eq!(
            shortcut_for_class(b"ghostty\0scratch.terminal\0"),
            "ctrl+shift+v"
        );
        assert_eq!(
            shortcut_for_class(b"other-instance\0com.mitchellh.ghostty\0"),
            "ctrl+shift+v"
        );
        assert_eq!(shortcut_for_class(b"chromium\0Chromium\0"), "ctrl+v");
        assert_eq!(shortcut_for_class(b"not-ghostty\0Other\0"), "ctrl+v");
    }
}
