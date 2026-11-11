//! The UI process (`murmur --ui setup|settings`).
//!
//! Threads:
//! - main: the Slint event loop;
//! - `ui-stdin`: reads [`ToUi`] lines and hands each state to the event loop;
//! - `ui-stdout`: writes [`FromUi`] lines, flushed one by one, so a full pipe never freezes the
//!   window.
//!
//! The process quits on [`ToUi::Close`], on stdin EOF, when the window is closed, after Done,
//! and when the daemon stops reading stdout. stdout carries only the protocol: logs go to stderr.
//!
//! A control the user can change (check box, combo box, slider) is only overwritten when the
//! daemon's value for it changes. Otherwise an unrelated update (e.g. download progress)
//! arriving before the daemon has processed the user's change would make the control jump back.

use std::{
    io::{self, BufRead, Write},
    sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc},
    thread,
    time::Duration,
};

use slint::{CloseRequestResponse, ComponentHandle, ModelRc, SharedString, VecModel};
use tracing::{debug, info, warn};

use super::components::{self as c, RowStatus, SettingsWindow, SetupWindow};
use super::protocol::{FromUi, ToUi, UiState, Window};
use super::view::{self, ModelAction, SettingsView, SetupView, ShortcutView, Status};

/// Wayland app id, so the desktop shows murmur's name and icon on the window.
pub const APP_ID: &str = "io.github.gabus.Murmur";

/// How long to wait for the daemon's first state before showing the window anyway.
const FIRST_STATE_GRACE: Duration = Duration::from_millis(500);

/// How long to wait at exit for the last messages (e.g. `SetupDone`) to reach the pipe.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error("windowing: {0}")]
    Platform(#[from] slint::PlatformError),
    #[error("event loop: {0}")]
    EventLoop(#[from] slint::EventLoopError),
    #[error("pipe to the daemon: {0}")]
    Io(#[from] io::Error),
    #[error("encoding a message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("offscreen rendering: {0}")]
    Offscreen(&'static str),
}

/// Runs the window until it's closed. Blocks the calling thread, which must be the main
/// thread. Installs a tracing subscriber that writes to stderr, so call it before any other
/// subscriber is set up: stdout is the protocol pipe.
pub fn run(window: Window) -> anyhow::Result<()> {
    init_tracing();
    info!(window = window.arg(), "UI starting");
    let (tx, rx) = mpsc::channel();
    let (flushed_tx, flushed_rx) = mpsc::channel::<()>();
    thread::Builder::new()
        .name("ui-stdout".into())
        .spawn(move || {
            if let Err(e) = write_messages(rx, io::stdout().lock()) {
                warn!(error = %e, "can't write to the daemon; closing the window");
                quit_soon();
            }
            let _ = flushed_tx.send(());
        })?;
    let out = Outbox(tx);
    let result = run_window(window, out.clone());
    // Write what's queued, then stop. Bounded, in case the daemon stopped reading.
    out.end();
    if flushed_rx.recv_timeout(FLUSH_TIMEOUT).is_err() {
        warn!("couldn't hand the last messages to the daemon");
    }
    info!("UI closed");
    Ok(result?)
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    // Fails only if a subscriber is already installed; then that one is used.
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            // zbus warns when there's no accessibility bus; that's not our problem to report.
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,zbus=error")),
        )
        .try_init();
}

fn run_window(window: Window, out: Outbox) -> Result<(), UiError> {
    match window {
        Window::Setup => {
            let ui = SetupWindow::new()?;
            let shown = Shown::default();
            wire_setup(&ui, &shown, &out);
            drop(out);
            run_page(ui, shown)
        }
        Window::Settings => {
            let ui = SettingsWindow::new()?;
            let shown = Shown::default();
            wire_settings(&ui, &shown, &out);
            drop(out);
            run_page(ui, shown)
        }
    }
}

// ------------------------------------------------------------------ pipe

/// Parses one line from the daemon. Blank lines are skipped (`Ok(None)`).
pub(super) fn parse_line(line: &[u8]) -> Result<Option<ToUi>, serde_json::Error> {
    let line = line.trim_ascii();
    if line.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(line).map(Some)
}

/// One message as one line, newline included.
pub(super) fn encode_line(msg: &FromUi) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    Ok(line)
}

/// Reads states until `Close` or EOF. Lines that don't parse are logged and skipped.
fn read_messages(input: impl BufRead, mut on_state: impl FnMut(UiState)) -> io::Result<()> {
    for line in input.split(b'\n') {
        match parse_line(&line?) {
            Ok(Some(ToUi::State(state))) => on_state(state),
            Ok(Some(ToUi::Close)) => {
                debug!("daemon asked to close");
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => warn!(error = %e, "ignoring a line from the daemon that doesn't parse"),
        }
    }
    debug!("stdin closed");
    Ok(())
}

enum Outgoing {
    Message(FromUi),
    /// Stop writing; sent once the window is gone.
    End,
}

/// Writes messages, flushing after each line, until [`Outgoing::End`] or every sender is gone.
fn write_messages(rx: mpsc::Receiver<Outgoing>, mut out: impl Write) -> Result<(), UiError> {
    for item in rx {
        let Outgoing::Message(msg) = item else { break };
        out.write_all(encode_line(&msg)?.as_bytes())?;
        out.flush()?;
    }
    Ok(())
}

/// Sends messages to the stdout writer.
#[derive(Clone)]
struct Outbox(mpsc::Sender<Outgoing>);

impl Outbox {
    fn send(&self, msg: FromUi) {
        debug!(?msg, "to daemon");
        if self.0.send(Outgoing::Message(msg)).is_err() {
            warn!("the daemon pipe is closed; dropping a message");
        }
    }

    /// Lets the writer finish after the messages already queued.
    fn end(&self) {
        let _ = self.0.send(Outgoing::End);
    }
}

/// Ends the event loop from any thread.
fn quit_soon() {
    // Errors mean the loop isn't running (any more): nothing to quit.
    let _ = slint::invoke_from_event_loop(|| {
        let _ = slint::quit_event_loop();
    });
}

// ------------------------------------------------------------------ pages

/// Both windows are this wide (`components.rs`: `min-width`/`max-width`).
const WINDOW_WIDTH: f32 = 520.0;

/// The view currently shown, shared between the event loop (writes) and callbacks (read).
type Shown<V> = Arc<Mutex<Option<V>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A window that displays a view derived from [`UiState`].
pub(super) trait Page: ComponentHandle + 'static {
    type View: Send + 'static;
    fn view(state: &UiState) -> Self::View;
    /// Copies `next` into the component. `prev` is what's shown now (`None` the first time).
    fn apply(&self, prev: Option<&Self::View>, next: &Self::View);
    /// The content's natural height (logical px); the window's height follows it.
    fn content_height(&self) -> f32;
    /// Calls `f` whenever the window's height changes.
    fn on_height_changed(&self, f: impl Fn() + 'static);
}

fn show_state<P: Page>(ui: &P, shown: &Shown<P::View>, next: P::View) {
    let mut slot = lock(shown);
    ui.apply(slot.as_ref(), &next);
    *slot = Some(next);
    fit(ui);
}

/// Asks for a window as tall as its content. The min/max height constraints alone aren't
/// enough: a change that lands while the compositor is still configuring the new window is
/// lost, and the window keeps its first height (content cut off, `tests/ui_nested_kwin.rs`).
/// Called after every state and whenever the compositor changes the window's height.
fn fit<P: Page>(ui: &P) {
    let window = ui.window();
    let height = ui.content_height();
    let current = window.size().to_logical(window.scale_factor()).height;
    debug!(window = current, content = height, "fit");
    if height > 0.0 && (current - height).abs() >= 1.0 {
        window.set_size(slint::LogicalSize::new(WINDOW_WIDTH, height));
    }
}

fn run_page<P: Page>(ui: P, shown: Shown<P::View>) -> Result<(), UiError> {
    // Takes effect when the window is created, so before show().
    if let Err(e) = slint::set_xdg_app_id(APP_ID) {
        warn!(error = %e, "can't set the app id");
    }
    ui.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        CloseRequestResponse::HideWindow
    });
    let weak = ui.as_weak();
    ui.on_height_changed(move || {
        if let Some(ui) = weak.upgrade() {
            fit(&ui);
        }
    });

    // The first state is handed over directly so the window never shows placeholder values.
    // Rendezvous channel: if we stop waiting, the reader gets the state back and delivers it
    // through the event loop instead.
    let (first_tx, first_rx) = mpsc::sync_channel::<UiState>(0);
    let weak = ui.as_weak();
    let reader_shown = shown.clone();
    thread::Builder::new()
        .name("ui-stdin".into())
        .spawn(move || {
            let mut first = Some(first_tx);
            let result = read_messages(io::stdin().lock(), |state| {
                let state = match first.take() {
                    Some(tx) => match tx.send(state) {
                        Ok(()) => return,
                        Err(mpsc::SendError(state)) => state,
                    },
                    None => state,
                };
                let next = P::view(&state);
                let shown = reader_shown.clone();
                if weak
                    .upgrade_in_event_loop(move |ui| show_state(&ui, &shown, next))
                    .is_err()
                {
                    debug!("event loop gone; dropping a state");
                }
            });
            if let Err(e) = result {
                warn!(error = %e, "can't read from the daemon");
            }
            quit_soon();
        })?;

    match first_rx.recv_timeout(FIRST_STATE_GRACE) {
        Ok(state) => show_state(&ui, &shown, P::view(&state)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            debug!("no state from the daemon yet; showing the window anyway");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // stdin ended (or Close arrived) before any state: nothing to show.
            info!("daemon closed the pipe before sending a state");
            return Ok(());
        }
    }
    drop(first_rx);

    ui.show()?;
    slint::run_event_loop_until_quit()?;
    ui.hide()?;
    Ok(())
}

fn status(s: Status) -> RowStatus {
    match s {
        Status::Todo => RowStatus::Todo,
        Status::Busy => RowStatus::Busy,
        Status::Done => RowStatus::Done,
        Status::Failed => RowStatus::Failed,
    }
}

fn text(s: &str) -> SharedString {
    SharedString::from(s)
}

fn strings(items: &[String]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        items.iter().map(|s| text(s)).collect::<Vec<_>>(),
    ))
}

fn index(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

fn changed<T: PartialEq>(prev: Option<&T>, next: &T) -> bool {
    prev != Some(next)
}

/// Sets the shortcut row's properties (same names on both windows).
macro_rules! apply_shortcut {
    ($ui:expr, $v:expr) => {{
        let v: &ShortcutView = $v;
        $ui.set_shortcut_status(status(v.status));
        $ui.set_shortcut_trigger(text(&v.trigger));
        $ui.set_shortcut_detail(text(&v.detail.text));
        $ui.set_shortcut_detail_error(v.detail.error);
        $ui.set_shortcut_can_change(v.can_change);
    }};
}

/// Whether the autostart check box should be overwritten: the daemon's value or its error
/// changed (a failed change keeps the value but sets the error, so the box flips back).
fn autostart_changed(prev: Option<(bool, &str)>, next: (bool, &str)) -> bool {
    prev != Some(next)
}

// ------------------------------------------------------------------ setup

impl Page for SetupWindow {
    type View = SetupView;

    fn content_height(&self) -> f32 {
        self.get_content_height()
    }

    fn on_height_changed(&self, f: impl Fn() + 'static) {
        self.on_resized(f);
    }

    fn view(state: &UiState) -> SetupView {
        view::setup(state)
    }

    fn apply(&self, prev: Option<&SetupView>, v: &SetupView) {
        if changed(prev.map(|p| &p.shortcut), &v.shortcut) {
            apply_shortcut!(self, &v.shortcut);
        }
        if changed(prev.map(|p| &p.typing), &v.typing) {
            self.set_typing_status(status(v.typing.status));
            self.set_typing_detail(text(&v.typing.detail.text));
            self.set_typing_detail_error(v.typing.detail.error);
            self.set_typing_button(text(&v.typing.button));
        }
        if changed(prev.map(|p| &p.model), &v.model) {
            let m = &v.model;
            self.set_model_status(status(m.status));
            self.set_model_ring(m.ring.unwrap_or(-1.0));
            self.set_model_detail(text(&m.detail.text));
            self.set_model_detail_error(m.detail.error);
            self.set_model_bar_visible(m.bar.is_some());
            let bar = m.bar.unwrap_or_default();
            self.set_model_bar_progress(bar.progress);
            self.set_model_bar_indeterminate(bar.indeterminate);
            self.set_model_action(text(m.action.label()));
        }
        if changed(prev.map(|p| &p.models), &v.models) {
            let items: Vec<c::ModelChoice> = v
                .models
                .iter()
                .map(|m| c::ModelChoice {
                    id: text(&m.id),
                    name: text(&m.name),
                    summary: text(&m.summary),
                    size: text(&m.size),
                    note: text(&m.note),
                    active: m.active,
                })
                .collect();
            self.set_models(ModelRc::new(VecModel::from(items)));
            let active = v.models.iter().position(|m| m.active);
            self.set_active_model(active.map_or(-1, index));
            self.set_active_model(v.models.iter().position(|m| m.active).map_or(-1, index));
        }
        if autostart_changed(
            prev.map(|p| (p.autostart, p.autostart_error.as_str())),
            (v.autostart, v.autostart_error.as_str()),
        ) {
            self.set_autostart(v.autostart);
            self.set_autostart_error(text(&v.autostart_error));
        }
        self.set_done_enabled(v.done_enabled);
        self.set_done_hint(text(&v.done_hint));
    }
}

fn wire_setup(ui: &SetupWindow, shown: &Shown<SetupView>, out: &Outbox) {
    let o = out.clone();
    ui.on_change_shortcut(move || o.send(FromUi::ChangeShortcut));
    let o = out.clone();
    ui.on_allow_typing(move || o.send(FromUi::AllowTyping));
    let o = out.clone();
    ui.on_select_model(move |id| o.send(FromUi::SelectModel(id.into())));
    let (o, s) = (out.clone(), shown.clone());
    ui.on_model_action_clicked(move || {
        let action = lock(&s).as_ref().map(|v| v.model.action.clone());
        match action {
            Some(ModelAction::Retry(id)) => o.send(FromUi::RetryDownload(id)),
            Some(ModelAction::Download(id)) => o.send(FromUi::SelectModel(id)),
            Some(ModelAction::None) | None => {}
        }
    });
    let o = out.clone();
    ui.on_autostart_toggled(move |on| o.send(FromUi::SetAutostart(on)));
    let o = out.clone();
    ui.on_done(move || {
        o.send(FromUi::SetupDone);
        // Setup is over; the daemon also sees stdout close.
        let _ = slint::quit_event_loop();
    });
}

// ------------------------------------------------------------------ settings

impl Page for SettingsWindow {
    type View = SettingsView;

    fn content_height(&self) -> f32 {
        self.get_content_height()
    }

    fn on_height_changed(&self, f: impl Fn() + 'static) {
        self.on_resized(f);
    }

    fn view(state: &UiState) -> SettingsView {
        view::settings(state)
    }

    fn apply(&self, prev: Option<&SettingsView>, v: &SettingsView) {
        if changed(prev.map(|p| &p.shortcut), &v.shortcut) {
            apply_shortcut!(self, &v.shortcut);
        }
        if changed(prev.map(|p| &p.microphone_labels), &v.microphone_labels) {
            self.set_microphone_labels(strings(&v.microphone_labels));
            self.set_microphone_index(index(v.microphone_index));
        } else if changed(prev.map(|p| &p.microphone_index), &v.microphone_index) {
            self.set_microphone_index(index(v.microphone_index));
        }
        if changed(prev.map(|p| &p.model_labels), &v.model_labels) {
            self.set_model_labels(strings(&v.model_labels));
            self.set_model_index(index(v.model_index));
        } else if changed(prev.map(|p| &p.model_index), &v.model_index) {
            self.set_model_index(index(v.model_index));
        }
        if changed(prev.map(|p| &p.download), &v.download) {
            let d = v.download.clone().unwrap_or_default();
            self.set_download_visible(v.download.is_some());
            self.set_download_text(text(&d.detail.text));
            self.set_download_error(d.detail.error);
            self.set_download_bar_visible(d.bar.is_some());
            let bar = d.bar.unwrap_or_default();
            self.set_download_progress(bar.progress);
            self.set_download_indeterminate(bar.indeterminate);
            self.set_download_retry(d.retry.is_some());
        }
        if changed(prev.map(|p| &p.pause_secs), &v.pause_secs) {
            self.set_pause_secs(v.pause_secs);
        }
        if changed(prev.map(|p| &p.idle_labels), &v.idle_labels) {
            self.set_idle_labels(strings(&v.idle_labels));
            self.set_idle_index(index(v.idle_index));
        } else if changed(prev.map(|p| &p.idle_index), &v.idle_index) {
            self.set_idle_index(index(v.idle_index));
        }
        if autostart_changed(
            prev.map(|p| (p.autostart, p.autostart_error.as_str())),
            (v.autostart, v.autostart_error.as_str()),
        ) {
            self.set_autostart(v.autostart);
            self.set_autostart_error(text(&v.autostart_error));
        }
        if changed(prev.map(|p| &p.system), &v.system) {
            let s = &v.system;
            // Two columns: base system on the left, desktop features on the right.
            let (left, right) = s.checks.split_at(s.checks.len().div_ceil(2));
            self.set_checks_left(system_checks(left));
            self.set_checks_right(system_checks(right));
            self.set_checks_summary(text(&s.summary));
            self.set_checks_all_ok(s.all_ok);
            self.set_checks_checking(s.checking);
        }
    }
}

fn system_checks(checks: &[super::checks::Check]) -> ModelRc<c::SystemCheck> {
    let items: Vec<c::SystemCheck> = checks
        .iter()
        .map(|ch| c::SystemCheck {
            title: text(ch.title),
            ok: ch.ok,
            pending: ch.pending,
            detail: text(&ch.detail),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

fn wire_settings(ui: &SettingsWindow, shown: &Shown<SettingsView>, out: &Outbox) {
    let o = out.clone();
    ui.on_change_shortcut(move || o.send(FromUi::ChangeShortcut));
    let (o, s) = (out.clone(), shown.clone());
    ui.on_microphone_selected(move |i| {
        let mic = lock(&s)
            .as_ref()
            .and_then(|v| v.microphone_at(i).map(str::to_owned));
        if let Some(mic) = mic {
            o.send(FromUi::SetMicrophone(mic));
        }
    });
    let (o, s) = (out.clone(), shown.clone());
    ui.on_model_selected(move |i| {
        let id = lock(&s)
            .as_ref()
            .and_then(|v| v.model_at(i).map(str::to_owned));
        if let Some(id) = id {
            o.send(FromUi::SelectModel(id));
        }
    });
    let (o, s) = (out.clone(), shown.clone());
    ui.on_retry_download(move || {
        let id = lock(&s)
            .as_ref()
            .and_then(|v| v.download.as_ref().and_then(|d| d.retry.clone()));
        if let Some(id) = id {
            o.send(FromUi::RetryDownload(id));
        }
    });
    let o = out.clone();
    ui.on_pause_released(move |secs| o.send(FromUi::SetPauseSecs(view::clamp_pause(secs))));
    let (o, s) = (out.clone(), shown.clone());
    ui.on_idle_selected(move |i| {
        let minutes = lock(&s).as_ref().and_then(|v| v.idle_at(i));
        if let Some(minutes) = minutes {
            o.send(FromUi::SetIdleMinutes(minutes));
        }
    });
    let o = out.clone();
    ui.on_autostart_toggled(move |on| o.send(FromUi::SetAutostart(on)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::samples;

    #[test]
    fn parses_states_close_and_blank_lines() {
        let state = samples::setup_fresh();
        let line = serde_json::to_string(&ToUi::State(state.clone())).expect("serialize");
        assert_eq!(
            parse_line(line.as_bytes()).expect("parse"),
            Some(ToUi::State(state))
        );
        assert_eq!(
            parse_line(br#"{"type":"close"}"#).expect("parse"),
            Some(ToUi::Close)
        );
        assert_eq!(parse_line(b"  \r").expect("blank"), None);
        assert!(parse_line(b"{not json").is_err());
        assert!(parse_line(b"\xff\xfe").is_err());
    }

    #[test]
    fn encodes_one_line_per_message() {
        let line = encode_line(&FromUi::SetMicrophone("Mic\nwith newline".into())).expect("encode");
        assert!(line.ends_with('\n'));
        assert_eq!(
            line.matches('\n').count(),
            1,
            "embedded newlines are escaped"
        );
        assert_eq!(
            encode_line(&FromUi::SetupDone).expect("encode"),
            "{\"type\":\"setup_done\"}\n"
        );
        assert_eq!(
            encode_line(&FromUi::SetPauseSecs(0.8)).expect("encode"),
            "{\"type\":\"set_pause_secs\",\"data\":0.8}\n"
        );
    }

    #[test]
    fn reader_skips_bad_lines_and_stops_at_close() {
        let fresh = serde_json::to_string(&ToUi::State(samples::setup_fresh())).expect("ser");
        let done = serde_json::to_string(&ToUi::State(samples::setup_done())).expect("ser");
        let after = serde_json::to_string(&ToUi::State(UiState::default())).expect("ser");
        let input = format!("{fresh}\n\ngarbage\n{done}\n{{\"type\":\"close\"}}\n{after}\n");
        let mut got = Vec::new();
        read_messages(input.as_bytes(), |s| got.push(s)).expect("read");
        assert_eq!(got, [samples::setup_fresh(), samples::setup_done()]);
    }

    #[test]
    fn reader_ends_at_eof_without_trailing_newline() {
        let line = serde_json::to_string(&ToUi::State(samples::settings())).expect("ser");
        let mut got = Vec::new();
        read_messages(line.as_bytes(), |s| got.push(s)).expect("read");
        assert_eq!(got, [samples::settings()]);
    }

    #[test]
    fn writer_writes_queued_lines_then_stops_at_end() {
        let (tx, rx) = mpsc::channel();
        let out = Outbox(tx);
        out.send(FromUi::AllowTyping);
        out.send(FromUi::SelectModel("moonshine-base-en".into()));
        out.send(FromUi::SetupDone);
        out.end();
        out.send(FromUi::ChangeShortcut); // after End: not written
        // `out` is still alive here, as the window's callbacks may be: End alone stops it.
        let mut buf = Vec::new();
        write_messages(rx, &mut buf).expect("write");
        let text = String::from_utf8(buf).expect("utf8");
        let msgs: Vec<FromUi> = text
            .lines()
            .map(|l| serde_json::from_str(l).expect("line"))
            .collect();
        assert_eq!(
            msgs,
            [
                FromUi::AllowTyping,
                FromUi::SelectModel("moonshine-base-en".into()),
                FromUi::SetupDone
            ]
        );
    }

    #[test]
    fn writer_reports_a_closed_pipe() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = mpsc::channel();
        Outbox(tx).send(FromUi::SetupDone);
        assert!(matches!(write_messages(rx, Closed), Err(UiError::Io(_))));
    }

    #[test]
    fn autostart_is_reapplied_when_the_daemon_reports_an_error() {
        assert!(autostart_changed(None, (false, "")));
        assert!(!autostart_changed(Some((false, "")), (false, "")));
        assert!(autostart_changed(Some((false, "")), (false, "refused")));
        assert!(autostart_changed(Some((false, "")), (true, "")));
    }
}
