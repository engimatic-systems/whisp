# Whisp

A small Rust/GTK dictation notepad for Linux/X11. Hold a key, speak, release,
and get an editable transcript on your clipboard—or pasted into the app you're
already using. No local speech model is required.

Whisp records through PulseAudio (including PipeWire's PulseAudio server) and
uploads the recording to OpenAI when you stop. It is designed for a small,
floating 512×384 window, but recording shortcuts work while another app has focus.

## Build and run

You need:

- Rust **1.92 or newer**, Cargo, a C toolchain, and `pkg-config`.
- GTK **4.8 or newer**, including development headers.
- FFmpeg with PulseAudio input support, and a working PulseAudio-compatible server.
- `paplay` for sounds and `xdotool` for automatic paste.
- The Codex CLI with a ChatGPT login, or an OpenAI API key (see below).

On Arch/Artix, install the build and runtime dependencies:

```sh
sudo pacman -S --needed base-devel rust gtk4 ffmpeg libpulse xdotool
```

Then clone and build:

```sh
mkdir -p ~/projects
cd ~/projects
git clone https://github.com/engimatic-systems/whisp.git
cd whisp
cargo build --release --locked
./scripts/whisp
```

You can also clone with `gh repo clone engimatic-systems/whisp`.
On other distributions, install the equivalent GTK development, compiler,
FFmpeg, PulseAudio client, and X11 utility packages. This is an **X11 application**;
native Wayland global shortcuts and paste are not implemented.

The launcher runs `target/release/whisp` relative to the checkout. No root
installation or Flatpak runtime is needed. Rebuild with the same Cargo command
after updating, then close and reopen Whisp to run the new binary. Build jobs
are limited to two in `.cargo/config.toml` to reduce peak memory use.

## Authentication

### Existing Codex sign-in (default)

Install the [Codex CLI](https://learn.chatgpt.com/docs/codex/cli), run
`codex login`, and sign in with your ChatGPT account. Whisp starts
`codex app-server` to obtain a token in memory; Codex manages credential storage
and refresh. Whisp does not save a copy of that token.

This mode uses the desktop app's **undocumented**
`https://chatgpt.com/backend-api/transcribe` endpoint. It has been tested with a
real account, but is not a supported public transcription API and may change.
The service chooses the model, so model and vocabulary controls are unavailable
in this mode. Whisp refreshes the token and retries once after an HTTP 401.

### OpenAI API key

Open **Settings** (the gear), select **OpenAI API key**, enter your key, and save.
Alternatively, set `OPENAI_API_KEY` in Whisp's environment; it takes precedence
over a saved key. This uses the public `/v1/audio/transcriptions` API, billed
separately from ChatGPT. Whisp never switches authentication or billing modes
automatically.

The default API model is `gpt-transcribe`. The model field is editable, and the
optional vocabulary/context hint is sent as the transcription `prompt`. Use it
for project names, technical vocabulary, or unusual spellings. See OpenAI's
[speech-to-text documentation](https://developers.openai.com/api/docs/guides/speech-to-text)
for supported models and options.

## Everyday use

| Control | Action |
| --- | --- |
| **Hold F8** | Record while held; release to stop and transcribe. |
| **F9** | Toggle recording on/off. |
| **Record / Ctrl+R** | Record, stop, or retry a saved recording from the window. |
| **Copy / Ctrl+Shift+C** | Copy the selected text, or the whole draft if nothing is selected. |
| **Ctrl+C** | Normal selection copy in the editor. |
| **Clear** | Empty the editor; Ctrl+Z can undo it. |
| **Esc** | Hide the window. |
| **F10** | Show/hide the window, if you install the i3 example below. |

F8 and F9 are captured directly through X11. **Whisp must be running**, but it
can be hidden or unfocused. Change these keys in Settings; optional separate
Start and Stop shortcuts are available too. Enter combinations such as `F8`,
`Ctrl+Shift+space`, or `Super+F9`; leave a field blank to disable it. Conflicts with
i3 or other applications are reported without replacing the previous bindings.
Caps Lock and Num Lock do not change the shortcuts, and key repeat does not
retrigger recording.

**Auto-copy** is on by default. Each new transcript replaces the system clipboard,
while the editor retains all accumulated paragraphs. Turn it off for manual copy.

**Auto-paste** is off by default. When enabled, it copies the new transcript and
sends a Paste shortcut to the application focused when transcription finishes.
Automatic mode detects Ghostty—including a custom scratch-terminal class—and
uses Ctrl+Shift+V there; other apps receive Ctrl+V. Fixed Ctrl+V, Ctrl+Shift+V,
and Shift+Insert modes are also available in Settings. Shift+Insert may use the
PRIMARY selection instead of CLIPBOARD, depending on the receiving app.
Auto-paste never sends Enter/Return. It skips Whisp's own windows and skips
pasting while a recording shortcut is still held; the transcript stays copied.

Sounds provide feedback without looking away:

- A high tone: recording started.
- A lower tone: recording stopped.
- A rising double chime: the new transcript is copied and ready to paste.
- A low descending double chime: an error needs attention in the window.

Disable sounds in Settings. The input meter shows peak microphone level on a
-60 to 0 dBFS scale, with a short decay; elapsed time appears below the editor.

Recordings **under one second** are discarded before upload. Recording stops and
transcribes automatically at **10 minutes**. Holding push-to-talk past the limit
does not start a new recording until you release the key. Releasing push-to-talk
does not stop a manual recording that was already active before the hold.

Hiding the window does **not** stop recording. Closing it exits, saves the draft,
and finalizes active audio locally for retry; closing does not initiate an upload.

## i3 integration

The [example configuration](examples/i3.conf) uses the included
[toggle helper](scripts/toggle-i3.sh). It needs `i3-msg`, `jq`, and `flock`
(`i3-wm`, `jq`, and `util-linux` on Arch/Artix).

Copy these lines into `~/.config/i3/config`, adjusting the checkout path:

```i3
for_window [class="^whisp$" title="^Whisp$"] floating enable, move scratchpad, resize set 512 px 384 px, scratchpad show
bindsym F10 exec --no-startup-id bash ~/projects/whisp/scripts/toggle-i3.sh
```

Reload i3 with `i3-msg reload`. F10 launches Whisp if absent, hides it if shown on
the current workspace, or summons it from elsewhere. The helper sets the window
to 512×384. Leave F8/F9 unbound in i3 when using Whisp's default native shortcuts.
The example also includes an optional login-start line.

## Saved data and recovery

| Path | Contents |
| --- | --- |
| `~/.config/whisp/settings.json` | Authentication mode, shortcuts, paste behavior, sounds, model, and vocabulary. |
| `~/.config/whisp/api-key` | Optional saved API key, mode 0600. |
| `~/.local/state/whisp/draft.txt` | Autosaved editable draft. |
| `~/.local/state/whisp/recording.wav` | Pending recording, removed after its transcript is saved. |

`XDG_CONFIG_HOME` and `XDG_STATE_HOME` override the base directories. Whisp's
application directories are mode 0700. Build artifacts and recordings are excluded
from Git; credentials and drafts live outside the checkout.

Network and transcription errors retain audio for **Retry transcription**. Retry
or explicitly discard that recording before starting another. Retrying after a
timeout may repeat a request that the server already processed, including any
API charge. The editor remains usable while transcription is running.

Capture uses the default PulseAudio microphone. Choose it with your usual mixer
(such as `pavucontrol`). Audio is mono, 16 kHz PCM WAV. If capture fails, check that
PipeWire's PulseAudio server is running and `ffmpeg -devices` lists `pulse`.
If the microphone meter stays flat, check the selected input and mute state.

## Scriptable controls

```sh
./scripts/whisp --start
./scripts/whisp --stop
./scripts/whisp --toggle
./scripts/whisp --ptt-start
./scripts/whisp --ptt-stop
./scripts/whisp --show
```

Commands forward to the running instance. Start/stop are idempotent; Stop does
not retry previously saved audio. Native shortcuts do not launch helper processes
or depend on i3. Launching without arguments shows the window.

## Development checks

```sh
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
bash -n scripts/whisp scripts/toggle-i3.sh
```

Normal tests use synthetic audio and mock HTTP responses: no microphone, login,
or paid requests. Explicitly ignored integration tests cover native X11 grabs,
clipboard/paste behavior, and a live ChatGPT transcription:

```sh
# Live desktop: temporarily grabs Ctrl+Shift+F6/F7.
cargo test --locked native_grabs_repeat_release_and_conflict_rollback -- --ignored --test-threads=1

# Live desktop, xdotool, Ghostty, and Python: opens disposable test windows.
cargo test --locked completion_copies_only_new_text_and_clear_is_undoable -- --ignored --test-threads=1

# Uses your Codex login and /usr/share/sounds/alsa/Front_Center.wav; contacts ChatGPT.
cargo test --locked live_codex_transcription -- --ignored --test-threads=1
```

## License

MIT; see [LICENSE](LICENSE).
