mod audio;
mod codex;
mod config;
mod hotkeys;
mod paste;
mod transcribe;

use anyhow::Result;
use audio::Recorder;
use config::{Backend, Paths, Settings, atomic_write};
use gtk::{gdk, gio, glib, prelude::*};
use std::{
    cell::RefCell,
    fs,
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

enum Outcome {
    Transcript(String),
    Discarded,
}

struct State {
    recorder: Option<Recorder>,
    job: Option<mpsc::Receiver<Result<Outcome>>>,
    paste_job: Option<mpsc::Receiver<Result<()>>>,
    settings: Settings,
    dirty_since: Option<Instant>,
    ptt_down: bool,
    ptt_recording: bool,
}

struct Ui {
    window: gtk::ApplicationWindow,
    text: gtk::TextView,
    record: gtk::Button,
    discard: gtk::Button,
    status: gtk::Label,
    meter: gtk::LevelBar,
    auto_copy: gtk::CheckButton,
    auto_paste: gtk::CheckButton,
    paths: Paths,
    state: RefCell<State>,
    hotkeys: RefCell<Option<hotkeys::Hotkeys>>,
}

fn padded_box(padding: i32) -> gtk::Box {
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 8);
    layout.set_margin_top(padding);
    layout.set_margin_bottom(padding);
    layout.set_margin_start(padding);
    layout.set_margin_end(padding);
    layout
}

impl Ui {
    fn new(app: &gtk::Application, paths: Paths) -> Rc<Self> {
        let (settings, settings_error) = match paths.settings() {
            Ok(settings) => (settings, None),
            Err(e) => (Settings::default(), Some(e.to_string())),
        };
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Whisp")
            .default_width(512)
            .default_height(384)
            .build();
        let layout = padded_box(10);
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let record = gtk::Button::with_label("● Record");
        record.set_tooltip_text(Some("Record / stop and transcribe · Ctrl+R"));
        record.add_css_class("suggested-action");
        let copy = gtk::Button::with_label("Copy");
        copy.set_tooltip_text(Some("Copy selection, or the whole draft · Ctrl+Shift+C"));
        let clear = gtk::Button::with_label("Clear");
        clear.set_tooltip_text(Some("Clear the draft · Ctrl+Z to undo"));
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        let settings_button = gtk::Button::from_icon_name("emblem-system-symbolic");
        settings_button.set_tooltip_text(Some("Settings"));
        for widget in [
            record.upcast_ref::<gtk::Widget>(),
            copy.upcast_ref(),
            clear.upcast_ref(),
            spacer.upcast_ref(),
            settings_button.upcast_ref(),
        ] {
            toolbar.append(widget);
        }
        layout.append(&toolbar);
        let feedback = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let auto_copy = gtk::CheckButton::with_label("Auto-copy");
        auto_copy.set_active(settings.auto_copy);
        auto_copy.set_tooltip_text(Some("Copy each new transcription to the system clipboard"));
        feedback.append(&auto_copy);
        let auto_paste = gtk::CheckButton::with_label("Auto-paste");
        auto_paste.set_active(settings.auto_paste);
        auto_paste.set_tooltip_text(Some(
            "Copy the new transcript and paste into the focused app when ready",
        ));
        feedback.append(&auto_paste);
        feedback.append(&gtk::Label::new(Some("Mic")));
        let meter = gtk::LevelBar::new();
        meter.set_hexpand(true);
        meter.set_valign(gtk::Align::Center);
        meter.set_tooltip_text(Some("Microphone input level · only active while recording"));
        feedback.append(&meter);
        layout.append(&feedback);
        let text = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .vexpand(true)
            .hexpand(true)
            .build();
        text.buffer()
            .set_text(&fs::read_to_string(paths.draft()).unwrap_or_default());
        text.buffer().set_enable_undo(true);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&text)
            .vexpand(true)
            .has_frame(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        layout.append(&scroll);
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();
        status.add_css_class("dim-label");
        layout.append(&status);
        let discard = gtk::Button::with_label("Discard saved recording");
        discard.set_visible(false);
        layout.append(&discard);
        let hint = gtk::Label::new(Some(
            "Ctrl+R record / stop    Ctrl+Shift+C copy    Esc hide",
        ));
        hint.add_css_class("dim-label");
        hint.add_css_class("caption");
        layout.append(&hint);
        window.set_child(Some(&layout));
        let ui = Rc::new(Self {
            window,
            text,
            record,
            discard,
            status,
            meter,
            auto_copy,
            auto_paste,
            paths,
            hotkeys: RefCell::new(None),
            state: RefCell::new(State {
                recorder: None,
                job: None,
                paste_job: None,
                settings,
                dirty_since: None,
                ptt_down: false,
                ptt_recording: false,
            }),
        });
        ui.idle();
        match hotkeys::Hotkeys::new().and_then(|mut keys| {
            keys.configure(&ui.state.borrow().settings.hotkeys)?;
            Ok(keys)
        }) {
            Ok(keys) => *ui.hotkeys.borrow_mut() = Some(keys),
            Err(e) => ui.show_error(&format!(
                "Global shortcuts unavailable: {e}. Open Settings to change them."
            )),
        }
        if let Some(error) = settings_error {
            ui.show_error(&error);
        }

        let weak = Rc::downgrade(&ui);
        ui.record.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.record_or_stop();
            }
        });
        let weak = Rc::downgrade(&ui);
        copy.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.copy();
            }
        });
        let weak = Rc::downgrade(&ui);
        clear.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.clear();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.auto_copy.connect_toggled(move |button| {
            if let Some(ui) = weak.upgrade() {
                let mut settings = ui.state.borrow().settings.clone();
                settings.auto_copy = button.is_active();
                match ui.paths.save_settings(&settings) {
                    Ok(()) => ui.state.borrow_mut().settings = settings,
                    Err(e) => ui.show_error(&format!("Could not save auto-copy setting: {e}")),
                }
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.auto_paste.connect_toggled(move |button| {
            if let Some(ui) = weak.upgrade() {
                let mut settings = ui.state.borrow().settings.clone();
                settings.auto_paste = button.is_active();
                match ui.paths.save_settings(&settings) {
                    Ok(()) => ui.state.borrow_mut().settings = settings,
                    Err(e) => ui.show_error(&format!("Could not save auto-paste setting: {e}")),
                }
            }
        });
        let weak = Rc::downgrade(&ui);
        settings_button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.settings();
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.discard.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                match fs::remove_file(ui.paths.recording()) {
                    Ok(()) => ui.idle(),
                    Err(e) => ui.show_error(&format!("Could not discard recording: {e}")),
                }
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.text.buffer().connect_changed(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.state.borrow_mut().dirty_since = Some(Instant::now());
            }
        });
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&ui);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(ui) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                && matches!(key, gdk::Key::r | gdk::Key::R)
            {
                ui.record_or_stop();
            } else if modifiers
                .contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK)
                && matches!(key, gdk::Key::c | gdk::Key::C)
            {
                ui.copy();
            } else if key == gdk::Key::Escape {
                ui.window.set_visible(false);
            } else {
                return glib::Propagation::Proceed;
            }
            glib::Propagation::Stop
        });
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        ui.window.add_controller(keys);
        // GTK stays on the main thread; capture shutdown and HTTPS use a worker.
        let tick_ui = Rc::clone(&ui);
        glib::timeout_add_local(Duration::from_millis(20), move || {
            tick_ui.tick();
            glib::ControlFlow::Continue
        });
        let weak = Rc::downgrade(&ui);
        app.connect_shutdown(move |_| {
            if let Some(ui) = weak.upgrade() {
                let _ = ui.save_draft();
                if let Some(recorder) = ui.state.borrow_mut().recorder.take() {
                    let _ = recorder.stop(); // Keep audio; do not upload on exit.
                }
            }
        });
        let app_weak = app.downgrade();
        ui.window.connect_close_request(move |_| {
            if let Some(app) = app_weak.upgrade() {
                app.quit();
            }
            glib::Propagation::Proceed
        });
        ui.window.present();
        ui.text.grab_focus();
        ui
    }

    fn draft(&self) -> String {
        let b = self.text.buffer();
        b.text(&b.start_iter(), &b.end_iter(), true).into()
    }
    fn save_draft(&self) -> Result<()> {
        atomic_write(&self.paths.draft(), self.draft().as_bytes())
    }

    fn show_error(&self, message: &str) {
        self.status.set_text(message);
        self.error_sound();
    }

    fn error_sound(&self) {
        if self.state.borrow().settings.recording_sounds {
            audio::ping(audio::Sound::Error);
        }
    }

    fn idle(&self) {
        self.meter.set_value(0.0);
        self.record.set_sensitive(true);
        self.record.remove_css_class("destructive-action");
        self.record.add_css_class("suggested-action");
        let pending = self.paths.recording().exists();
        self.record.set_label(if pending {
            "Retry transcription"
        } else {
            "● Record"
        });
        self.discard.set_visible(pending);
        self.status.set_text(if pending {
            "A saved recording is ready to retry."
        } else if self.state.borrow().settings.backend == Backend::Codex {
            "Ready · using your Codex ChatGPT sign-in."
        } else if self.paths.api_key().is_err() {
            "Add your OpenAI API key in Settings to start."
        } else {
            "Ready · audio goes to OpenAI when you stop."
        });
    }

    fn copy(&self) {
        let buffer = self.text.buffer();
        let value = match buffer.selection_bounds() {
            Some((start, end)) => buffer.text(&start, &end, true).to_string(),
            None => self.draft(),
        };
        self.window.clipboard().set_text(&value);
        if self.state.borrow().recorder.is_none() && self.state.borrow().job.is_none() {
            self.status.set_text("Copied.");
        }
    }

    fn clear(&self) {
        let buffer = self.text.buffer();
        buffer.begin_user_action();
        buffer.delete(&mut buffer.start_iter(), &mut buffer.end_iter());
        buffer.end_user_action();
        self.text.grab_focus();
    }

    fn record_or_stop(self: &Rc<Self>) {
        if self.state.borrow().job.is_some() {
            return;
        }
        if self.state.borrow().recorder.is_some() || self.paths.recording().exists() {
            self.submit();
        } else {
            self.start_recording();
        }
    }

    fn start_recording(self: &Rc<Self>) {
        if self.state.borrow().recorder.is_some() || self.state.borrow().job.is_some() {
            return;
        }
        if self.paths.recording().exists() {
            self.status
                .set_text("Retry or discard the saved recording before starting another.");
            return;
        }
        if self.state.borrow().settings.backend == Backend::ApiKey
            && let Err(e) = self.paths.api_key()
        {
            self.show_error(&e.to_string());
            self.settings();
            return;
        }
        match Recorder::start(self.paths.recording()) {
            Ok(recorder) => {
                self.state.borrow_mut().recorder = Some(recorder);
                self.record.set_label("■ Stop & transcribe");
                self.record.remove_css_class("suggested-action");
                self.record.add_css_class("destructive-action");
                self.status.set_text("Recording… 0:00 · stops at 10:00");
                if self.state.borrow().settings.recording_sounds {
                    audio::ping(audio::Sound::Start);
                }
            }
            Err(e) => self.show_error(&format!("{e:#}")),
        }
    }

    fn stop_recording(&self) {
        if self.state.borrow().recorder.is_some() {
            self.submit();
        }
    }

    fn ptt_start(self: &Rc<Self>) {
        if self.state.borrow().ptt_down {
            return;
        }
        self.state.borrow_mut().ptt_down = true;
        if self.state.borrow().recorder.is_some() || self.state.borrow().job.is_some() {
            return;
        }
        self.start_recording();
        let active = self.state.borrow().recorder.is_some();
        self.state.borrow_mut().ptt_recording = active;
    }

    fn ptt_stop(&self) {
        let owned = {
            let mut state = self.state.borrow_mut();
            state.ptt_down = false;
            std::mem::take(&mut state.ptt_recording)
        };
        if owned {
            self.stop_recording();
        }
    }

    fn submit(&self) {
        let paths = self.paths.clone();
        let (sender, receiver) = mpsc::channel();
        let mut state = self.state.borrow_mut();
        let recorder = state.recorder.take();
        state.ptt_recording = false;
        let settings = state.settings.clone();
        let path = self.paths.recording();
        state.job = Some(receiver);
        drop(state);
        self.meter.set_value(0.0);
        self.record.set_sensitive(false);
        self.discard.set_visible(false);
        self.record.set_label("Transcribing…");
        self.status
            .set_text("Transcribing with OpenAI… You can keep editing.");
        thread::spawn(move || {
            let result = (|| {
                if let Some(recorder) = recorder {
                    let tap = recorder.started.elapsed() < Duration::from_secs(1);
                    let stopped = recorder.stop();
                    if settings.recording_sounds {
                        audio::ping(audio::Sound::Stop);
                    }
                    // A tap can finish before FFmpeg has even produced a WAV header.
                    if tap {
                        fs::remove_file(&path)?;
                        return Ok(Outcome::Discarded);
                    }
                    if audio::discard_short_recording(&path).unwrap_or(false) {
                        return Ok(Outcome::Discarded);
                    }
                    stopped?;
                } else if audio::discard_short_recording(&path)? {
                    return Ok(Outcome::Discarded);
                }
                transcribe::transcribe(&path, &paths, &settings).map(Outcome::Transcript)
            })();
            let _ = sender.send(result);
        });
    }

    fn begin_paste(&self) {
        // Do not paste back into Whisp, or synthesize keys while a PTT grab is held.
        if self
            .window
            .application()
            .is_some_and(|app| app.windows().iter().any(|w| w.is_active()))
        {
            self.status
                .set_text("Copied · auto-paste skipped while Whisp has focus.");
            return;
        }
        if self.state.borrow().ptt_down
            || self
                .hotkeys
                .borrow()
                .as_ref()
                .is_some_and(|keys| keys.is_pressed())
        {
            self.status
                .set_text("Copied · release the recording shortcut and paste manually.");
            return;
        }
        let shortcut = self.state.borrow().settings.paste_shortcut.clone();
        let (sender, receiver) = mpsc::channel();
        self.state.borrow_mut().paste_job = Some(receiver);
        thread::spawn(move || {
            let _ = sender.send(paste::paste(&shortcut));
        });
    }

    fn dispatch(self: &Rc<Self>, action: hotkeys::Action) {
        match action {
            hotkeys::Action::Start => self.start_recording(),
            hotkeys::Action::Stop => self.stop_recording(),
            hotkeys::Action::Toggle => {
                if self.state.borrow().recorder.is_some() {
                    self.stop_recording();
                } else {
                    self.start_recording();
                }
            }
            hotkeys::Action::PttStart => self.ptt_start(),
            hotkeys::Action::PttStop => self.ptt_stop(),
        }
    }

    fn tick(self: &Rc<Self>) {
        let events = self.hotkeys.borrow_mut().as_mut().map(|keys| keys.poll());
        match events {
            Some(Ok(actions)) => {
                for action in actions {
                    self.dispatch(action);
                }
            }
            Some(Err(e)) => {
                // Losing the X11 connection must not leave push-to-talk recording.
                self.ptt_stop();
                self.hotkeys.borrow_mut().take();
                self.show_error(&format!("Global shortcuts stopped: {e}"));
            }
            None => {}
        }
        let stop = {
            let mut state = self.state.borrow_mut();
            if let Some(recorder) = &mut state.recorder {
                let seconds = recorder.started.elapsed().as_secs();
                self.meter
                    .set_value(recorder.level().max(self.meter.value() * 0.93));
                self.status.set_text(&format!(
                    "Recording… {}:{:02} · stops at 10:00",
                    seconds / 60,
                    seconds % 60
                ));
                recorder.finished() || seconds >= audio::MAX_SECONDS
            } else {
                false
            }
        };
        if stop {
            self.submit();
        }
        let result = self
            .state
            .borrow()
            .job
            .as_ref()
            .and_then(|job| match job.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!(
                    "Transcription worker stopped. Recording saved for retry."
                ))),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some(result) = result {
            self.state.borrow_mut().job = None;
            match result {
                Ok(Outcome::Discarded) => {
                    self.idle();
                    self.status
                        .set_text("Recording under one second discarded · ready.");
                }
                Ok(Outcome::Transcript(transcript)) => {
                    let buffer = self.text.buffer();
                    let prefix = if self.draft().trim().is_empty() {
                        ""
                    } else {
                        "\n\n"
                    };
                    buffer.begin_user_action();
                    buffer.insert(&mut buffer.end_iter(), &format!("{prefix}{transcript}"));
                    buffer.end_user_action();
                    let copied = self.auto_copy.is_active() || self.auto_paste.is_active();
                    if copied {
                        self.window.clipboard().set_text(&transcript);
                        gtk::prelude::WidgetExt::display(&self.window).flush();
                        if self.state.borrow().settings.recording_sounds {
                            audio::ping(audio::Sound::Ready);
                        }
                    }
                    // Preserve audio until the transcript is safely on disk.
                    match self
                        .save_draft()
                        .and_then(|_| Ok(fs::remove_file(self.paths.recording())?))
                    {
                        Ok(()) => {
                            self.state.borrow_mut().dirty_since = None;
                            self.idle();
                            self.status.set_text(if copied {
                                "Transcribed · copied to clipboard."
                            } else {
                                "Transcribed · select, edit, or copy."
                            });
                            if self.auto_paste.is_active() {
                                self.begin_paste();
                            }
                        }
                        Err(e) => {
                            self.idle();
                            self.show_error(&format!(
                                "Text received, but could not finish saving: {e}. Audio retained."
                            ));
                        }
                    }
                }
                Err(e) => {
                    self.idle();
                    self.show_error(&format!("{e:#}\nRecording saved. Retry or discard it."));
                }
            }
        }
        let pasted = self
            .state
            .borrow()
            .paste_job
            .as_ref()
            .and_then(|job| match job.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!(
                    "Auto-paste stopped. Transcript copied; paste manually."
                ))),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some(result) = pasted {
            self.state.borrow_mut().paste_job = None;
            if self.state.borrow().recorder.is_none() {
                match result {
                    Ok(()) => self
                        .status
                        .set_text("Transcribed · copied and paste shortcut sent."),
                    Err(e) => self.show_error(&format!("{e:#}")),
                }
            }
        }
        let save = self
            .state
            .borrow()
            .dirty_since
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(750));
        if save {
            self.state.borrow_mut().dirty_since = None;
            if let Err(e) = self.save_draft() {
                self.show_error(&format!("Draft could not be saved: {e}"));
            }
        }
    }

    fn settings(self: &Rc<Self>) {
        let dialog = gtk::Window::builder()
            .title("Whisp Settings")
            .transient_for(&self.window)
            .modal(true)
            .default_width(420)
            .resizable(false)
            .build();
        let layout = padded_box(16);
        let backend = gtk::DropDown::from_strings(&["Codex ChatGPT sign-in", "OpenAI API key"]);
        backend.set_selected(if self.state.borrow().settings.backend == Backend::Codex {
            0
        } else {
            1
        });
        layout.append(&backend);
        let auth_help = gtk::Label::builder().xalign(0.0).wrap(true).max_width_chars(48)
            .label("Uses your existing Codex ChatGPT sign-in. If needed, sign in with codex login in a terminal. The transcription model is selected automatically.").build();
        auth_help.set_visible(backend.selected() == 0);
        layout.append(&auth_help);
        let api_fields = gtk::Box::new(gtk::Orientation::Vertical, 8);
        layout.append(&api_fields);
        let key = gtk::PasswordEntry::builder().show_peek_icon(true).build();
        api_fields.append(
            &gtk::Label::builder()
                .label("OpenAI API key")
                .xalign(0.0)
                .build(),
        );
        api_fields.append(&key);
        let help = gtk::Label::builder().xalign(0.0).wrap(true).max_width_chars(48)
            .label("Leave blank to keep your saved key. API usage is billed separately from ChatGPT. Saved locally, readable only by your user.").build();
        help.add_css_class("dim-label");
        api_fields.append(&help);
        let model = gtk::Entry::builder()
            .text(&self.state.borrow().settings.model)
            .build();
        api_fields.append(
            &gtk::Label::builder()
                .label("Transcription model")
                .xalign(0.0)
                .build(),
        );
        api_fields.append(&model);
        let prompt = gtk::Entry::builder()
            .text(&self.state.borrow().settings.prompt)
            .placeholder_text("Optional: QEMU, project names, Tibetan terms…")
            .build();
        api_fields.append(
            &gtk::Label::builder()
                .label("Vocabulary / context hint")
                .xalign(0.0)
                .build(),
        );
        api_fields.append(&prompt);
        api_fields.set_visible(backend.selected() == 1);
        backend.connect_selected_notify(move |backend| {
            api_fields.set_visible(backend.selected() == 1);
            auth_help.set_visible(backend.selected() == 0);
        });
        let bindings = self.state.borrow().settings.hotkeys.clone();
        let mut shortcut_entries = Vec::new();
        for (label, value) in [
            ("Toggle recording", &bindings.toggle),
            ("Push to talk (hold)", &bindings.push_to_talk),
            ("Start recording (optional)", &bindings.start),
            ("Stop recording (optional)", &bindings.stop),
        ] {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let label = gtk::Label::builder()
                .label(label)
                .xalign(0.0)
                .hexpand(true)
                .build();
            let entry = gtk::Entry::builder().text(value).width_chars(15).build();
            row.append(&label);
            row.append(&entry);
            layout.append(&row);
            shortcut_entries.push(entry);
        }
        layout.append(&gtk::Label::builder().label("Global X11 shortcuts: F8, Ctrl+F9, Shift+F9… Leave blank to disable. Whisp must be running.").wrap(true).xalign(0.0).max_width_chars(48).build());
        layout.append(
            &gtk::Label::builder()
                .label("Auto-paste shortcut")
                .xalign(0.0)
                .build(),
        );
        let paste_shortcut = gtk::DropDown::from_strings(&[
            "Automatic (Ghostty / other apps)",
            "Ctrl+V (most applications)",
            "Ctrl+Shift+V (terminals)",
            "Shift+Insert",
        ]);
        paste_shortcut.set_selected(
            paste::SHORTCUTS
                .iter()
                .position(|s| *s == self.state.borrow().settings.paste_shortcut)
                .unwrap_or(0) as u32,
        );
        layout.append(&paste_shortcut);
        let sounds =
            gtk::CheckButton::with_label("Play recording, clipboard-ready, and error sounds");
        sounds.set_active(self.state.borrow().settings.recording_sounds);
        layout.append(&sounds);
        let error = gtk::Label::builder().wrap(true).xalign(0.0).build();
        layout.append(&error);
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
        layout.append(&save);
        dialog.set_child(Some(&layout));
        let weak = Rc::downgrade(self);
        let close = dialog.clone();
        save.connect_clicked(move |_| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let settings = Settings {
                backend: if backend.selected() == 0 {
                    Backend::Codex
                } else {
                    Backend::ApiKey
                },
                auto_copy: ui.auto_copy.is_active(),
                auto_paste: ui.auto_paste.is_active(),
                paste_shortcut: paste::SHORTCUTS[paste_shortcut.selected() as usize].into(),
                hotkeys: hotkeys::Bindings {
                    toggle: shortcut_entries[0].text().trim().to_owned(),
                    push_to_talk: shortcut_entries[1].text().trim().to_owned(),
                    start: shortcut_entries[2].text().trim().to_owned(),
                    stop: shortcut_entries[3].text().trim().to_owned(),
                },
                recording_sounds: sounds.is_active(),
                model: model.text().trim().to_owned(),
                prompt: prompt.text().trim().to_owned(),
            };
            if settings.backend == Backend::ApiKey && settings.model.is_empty() {
                error.set_text("Enter a transcription model.");
                return;
            }
            let secret = key.text().trim().to_owned();
            if secret.contains(['\r', '\n']) {
                error.set_text("The API key must be a single line.");
                return;
            }
            let old_bindings = ui.state.borrow().settings.hotkeys.clone();
            let result = (|| -> Result<()> {
                let mut keys = ui.hotkeys.borrow_mut();
                if keys.is_none() {
                    *keys = Some(hotkeys::Hotkeys::new()?);
                }
                keys.as_mut().unwrap().configure(&settings.hotkeys)?;
                if !secret.is_empty() {
                    atomic_write(&ui.paths.key_path(), secret.as_bytes())?;
                }
                ui.paths.save_settings(&settings)
            })();
            match result {
                Ok(()) => {
                    ui.state.borrow_mut().settings = settings;
                    if ui.state.borrow().recorder.is_none() && ui.state.borrow().job.is_none() {
                        ui.idle();
                    }
                    close.close();
                }
                Err(e) => {
                    if let Some(keys) = ui.hotkeys.borrow_mut().as_mut() {
                        let _ = keys.configure(&old_bindings);
                    }
                    error.set_text(&format!("Could not save settings: {e}"));
                    ui.error_sound();
                }
            }
        });
        dialog.present();
    }
}

fn main() -> glib::ExitCode {
    glib::set_prgname(Some("whisp"));
    glib::set_application_name("Whisp");
    let app = gtk::Application::builder()
        .application_id("local.whisp.Notepad")
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    let ui: Rc<RefCell<Option<Rc<Ui>>>> = Rc::new(RefCell::new(None));
    app.connect_command_line(move |app, command| {
        let args = command.arguments();
        let action = args.get(1).and_then(|s| s.to_str()).unwrap_or("--show");
        if action == "--help" {
            print!("Whisp: --show | --start | --stop | --toggle | --ptt-start | --ptt-stop\nControl the running instance from any application. With no arguments, show the window.\n");
            return glib::ExitCode::SUCCESS;
        }
        if args.len() > 2 || !matches!(action, "--show" | "--start" | "--stop" | "--toggle" | "--ptt-start" | "--ptt-stop") {
            eprintln!("Unknown Whisp command. Use --help.");
            return glib::ExitCode::FAILURE;
        }
        if ui.borrow().is_none() {
            // A stray release must not launch a window or retry old audio.
            if matches!(action, "--stop" | "--ptt-stop") { return glib::ExitCode::SUCCESS; }
            match Paths::discover() {
                Ok(paths) => *ui.borrow_mut() = Some(Ui::new(app, paths)),
                Err(e) => {
                    eprintln!("Whisp: {e:#}");
                    return glib::ExitCode::FAILURE;
                }
            }
        }
        let ui = ui.borrow().as_ref().unwrap().clone();
        match action {
            "--show" => ui.window.present(),
            "--start" => ui.start_recording(),
            "--stop" => ui.stop_recording(),
            "--toggle" => {
                if ui.state.borrow().recorder.is_some() { ui.stop_recording(); }
                else { ui.start_recording(); }
            }
            "--ptt-start" => ui.ptt_start(),
            "--ptt-stop" => ui.ptt_stop(),
            _ => unreachable!(),
        }
        glib::ExitCode::SUCCESS
    });
    app.run()
}

#[cfg(test)]
mod ui_tests {
    use super::*;

    #[test]
    #[ignore = "Uses the live GTK display and clipboard; restores the prior clipboard text"]
    fn completion_copies_only_new_text_and_clear_is_undoable() {
        gtk::init().unwrap();
        let app = gtk::Application::builder()
            .application_id("local.whisp.UiTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().to_owned(),
            state: dir.path().to_owned(),
        };
        let settings = Settings {
            hotkeys: hotkeys::Bindings {
                toggle: String::new(),
                push_to_talk: String::new(),
                start: String::new(),
                stop: String::new(),
            },
            recording_sounds: false,
            ..Settings::default()
        };
        paths.save_settings(&settings).unwrap();
        let ui = Ui::new(&app, paths.clone());
        ui.window.set_title(Some("Whisp UI check"));
        ui.window.set_visible(false);
        let clipboard = ui.window.clipboard();
        let context = glib::MainContext::default();
        let previous = context
            .block_on(clipboard.read_text_future())
            .ok()
            .flatten();
        struct Restore(gdk::Clipboard, Option<glib::GString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Some(text) = &self.1 {
                    self.0.set_text(text);
                }
            }
        }
        let _restore = Restore(clipboard.clone(), previous);
        ui.text.buffer().set_text("Previous paragraph.");
        let deliver = |text: &str| {
            fs::write(paths.recording(), b"saved audio").unwrap();
            let (sender, receiver) = mpsc::channel();
            sender
                .send(Ok(Outcome::Transcript(text.to_owned())))
                .unwrap();
            ui.state.borrow_mut().job = Some(receiver);
            ui.tick();
        };
        deliver("New dictation.");
        assert_eq!(ui.draft(), "Previous paragraph.\n\nNew dictation.");
        assert_eq!(
            context
                .block_on(clipboard.read_text_future())
                .unwrap()
                .as_deref(),
            Some("New dictation.")
        );
        assert_eq!(fs::read_to_string(paths.draft()).unwrap(), ui.draft());
        assert!(!paths.recording().exists());
        ui.auto_copy.set_active(false);
        assert!(!paths.settings().unwrap().auto_copy);
        deliver("Another paragraph.");
        assert_eq!(
            context
                .block_on(clipboard.read_text_future())
                .unwrap()
                .as_deref(),
            Some("New dictation.")
        );
        let draft = ui.draft();
        ui.clear();
        assert!(ui.draft().is_empty());
        ui.text.buffer().undo();
        assert_eq!(ui.draft(), draft);
        // Stop and PTT release while idle never submit a saved recording.
        fs::write(paths.recording(), b"retry me").unwrap();
        ui.stop_recording();
        ui.ptt_stop();
        assert!(ui.state.borrow().job.is_none());
        assert_eq!(fs::read(paths.recording()).unwrap(), b"retry me");
        // A discarded tap resets the UI without changing draft or clipboard.
        let (sender, receiver) = mpsc::channel();
        fs::remove_file(paths.recording()).unwrap();
        sender.send(Ok(Outcome::Discarded)).unwrap();
        ui.state.borrow_mut().job = Some(receiver);
        ui.tick();
        assert!(!ui.discard.is_visible());
        assert_eq!(ui.draft(), draft);

        // Paste into a separate, explicitly focused test editor, never the user's app.
        let target_text = gtk::TextView::new();
        let target = gtk::Window::builder()
            .title("Whisp paste test target")
            .default_width(360)
            .default_height(160)
            .child(&target_text)
            .build();
        target.present();
        target_text.grab_focus();
        let pump_until = |condition: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !condition() && Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert!(condition(), "Timed out waiting for GTK / paste event");
        };
        pump_until(&|| target.is_mapped());
        let window_id = std::process::Command::new("xdotool")
            .args([
                "search",
                "--onlyvisible",
                "--name",
                "^Whisp paste test target$",
            ])
            .output()
            .unwrap();
        let window_id = String::from_utf8(window_id.stdout).unwrap();
        assert!(
            std::process::Command::new("xdotool")
                .args(["windowactivate", "--sync", window_id.trim()])
                .status()
                .unwrap()
                .success()
        );
        pump_until(&|| target.is_active());
        ui.auto_paste.set_active(true);
        assert!(paths.settings().unwrap().auto_paste);
        assert!(!ui.auto_copy.is_active()); // Auto-paste still provides clipboard text.
        deliver("Paste test — བོད་\nsecond line");
        pump_until(&|| {
            target_text.buffer().char_count() > 0 && ui.state.borrow().paste_job.is_none()
        });
        assert_eq!(
            target_text
                .buffer()
                .text(
                    &target_text.buffer().start_iter(),
                    &target_text.buffer().end_iter(),
                    true
                )
                .as_str(),
            "Paste test — བོད་\nsecond line"
        );
        target.close();

        // A real Ghostty process runs a raw input collector, never a shell.
        // The custom class also exercises scratch-terminal detection by instance.
        let receiver_script = dir.path().join("collect-paste.py");
        let received_path = dir.path().join("received.bin");
        let ready_path = dir.path().join("ready");
        fs::write(
            &receiver_script,
            r#"import os,sys,tty,select,time
from pathlib import Path
tty.setraw(0)
os.write(1, b'\x1b[?2004h')
Path(sys.argv[2]).touch()
data=b''
deadline=time.monotonic()+10
while time.monotonic()<deadline:
 if select.select([0],[],[],0.1)[0]:
  data+=os.read(0,4096)
  if b'\x1b[201~' in data: break
Path(sys.argv[1]).write_bytes(data)
"#,
        )
        .unwrap();
        let ghostty = std::process::Command::new("ghostty")
            .args([
                "--gtk-single-instance=false",
                "--class=whisp.paste-test",
                "--title=Whisp paste integration check",
                "-e",
                "python",
            ])
            .arg(&receiver_script)
            .arg(&received_path)
            .arg(&ready_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        struct StopChild(std::process::Child);
        impl Drop for StopChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _ghostty = StopChild(ghostty);
        eprintln!("Waiting for Ghostty input collector");
        pump_until(&|| ready_path.exists());
        let window = std::process::Command::new("xdotool")
            .args(["search", "--onlyvisible", "--class", "^whisp.paste-test$"])
            .output()
            .unwrap();
        let window = String::from_utf8(window.stdout).unwrap();
        assert!(
            !window.trim().is_empty(),
            "Ghostty test window must exist before paste"
        );
        assert!(
            std::process::Command::new("xdotool")
                .args(["windowactivate", "--sync", window.trim()])
                .status()
                .unwrap()
                .success()
        );
        // Use the same completion path as actual dictation.
        deliver("Ghostty paste — བོད་\nsecond line");
        eprintln!("Waiting for Ghostty clipboard paste");
        pump_until(&|| received_path.exists());
        assert_eq!(
            fs::read(&received_path).unwrap(),
            "\x1b[200~Ghostty paste — བོད་\nsecond line\x1b[201~".as_bytes()
        );
        ui.auto_paste.set_active(false);
        ui.window.close();
    }
}
