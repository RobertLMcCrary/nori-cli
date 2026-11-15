use std::fmt;
use std::io::IsTerminal;
use std::io::Result;
use std::io::stdin;
use std::io::stdout;
use std::panic;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crossterm::Command;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableFocusChange;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::EnableFocusChange;
use crossterm::event::Event;
use crossterm::event::KeyEvent;
use crossterm::event::KeyboardEnhancementFlags;
use crossterm::event::PopKeyboardEnhancementFlags;
use crossterm::event::PushKeyboardEnhancementFlags;
use crossterm::terminal::BeginSynchronizedUpdate;
use crossterm::terminal::EndSynchronizedUpdate;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use crossterm::terminal::supports_keyboard_enhancement;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::execute;
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::disable_raw_mode;
use ratatui::crossterm::terminal::enable_raw_mode;
use ratatui::layout::Offset;
use ratatui::text::Line;
use tokio::select;
use tokio_stream::Stream;

use crate::custom_terminal;
use crate::custom_terminal::Terminal as CustomTerminal;
use crate::frame_writer::FrameWriter;
#[cfg(unix)]
use crate::tui::job_control::SUSPEND_KEY;
#[cfg(unix)]
use crate::tui::job_control::SuspendContext;

mod history_queue;
#[cfg(unix)]
mod job_control;
mod repaint;

/// A type alias for the terminal type used in this application
pub type Terminal = CustomTerminal<CrosstermBackend<FrameWriter>>;

pub fn set_modes() -> Result<()> {
    execute!(stdout(), EnableBracketedPaste)?;

    enable_raw_mode()?;
    // Enable keyboard enhancement flags so modifiers for keys like Enter are disambiguated.
    // chat_composer.rs is using a keyboard event listener to enter for any modified keys
    // to create a new line that require this.
    // Some terminals (notably legacy Windows consoles) do not support
    // keyboard enhancement flags. Attempt to enable them, but continue
    // gracefully if unsupported.
    let _ = execute!(
        stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );

    let _ = execute!(stdout(), EnableFocusChange);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> Result<()> {
        Err(std::io::Error::other(
            "tried to execute EnableAlternateScroll using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> Result<()> {
        Err(std::io::Error::other(
            "tried to execute DisableAlternateScroll using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Restore the terminal to its original state.
/// Inverse of `set_modes`.
pub fn restore() -> Result<()> {
    // Pop may fail on platforms that didn't support the push; ignore errors.
    let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    execute!(stdout(), DisableBracketedPaste)?;
    let _ = execute!(stdout(), DisableFocusChange);
    // Captured only while the transcript overlay is open, which an exit may cut short.
    let _ = execute!(stdout(), crossterm::event::DisableMouseCapture);
    disable_raw_mode()?;
    let _ = execute!(stdout(), crossterm::cursor::Show);
    Ok(())
}

/// Path to the controlling terminal, used for input when stdin is redirected.
#[cfg(unix)]
const CONTROLLING_TERMINAL: &str = "/dev/tty";
#[cfg(windows)]
const CONTROLLING_TERMINAL: &str = "CONIN$";

/// Initialize the terminal (inline viewport; history stays in normal scrollback)
pub fn init() -> Result<Terminal> {
    if !stdout().is_terminal() {
        return Err(std::io::Error::other("stdout is not a terminal"));
    }
    // stdin is allowed to be a pipe so `echo "prompt" | nori` can seed a session.
    // crossterm reads keys from the controlling terminal whenever stdin is not a
    // tty, so the only hard requirement is that one is actually attached.
    if !stdin().is_terminal() {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(CONTROLLING_TERMINAL)
            .map_err(|error| {
                std::io::Error::other(format!(
                    "stdin is not a terminal and no controlling terminal is available: {error}"
                ))
            })?;
    }
    set_modes()?;
    crate::startup_trace::mark("terminal_modes");

    set_panic_hook();

    let backend = CrosstermBackend::new(FrameWriter::stdout()?);
    let tui = CustomTerminal::with_options(backend)?;
    crate::startup_trace::mark("cursor_position");
    Ok(tui)
}

fn set_panic_hook() {
    let hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        let _ = restore(); // ignore any errors as we are already failing
        hook(panic_info);
    }));
}

#[derive(Debug)]
pub enum TuiEvent {
    Key(KeyEvent),
    Paste(String),
    /// Clicks and the wheel, reported only while an overlay captures the mouse.
    Mouse(crossterm::event::MouseEvent),
    Draw,
}

pub struct Tui {
    frame_schedule_tx: tokio::sync::mpsc::UnboundedSender<Instant>,
    draw_tx: tokio::sync::broadcast::Sender<()>,
    pub(crate) terminal: Terminal,
    history: history_queue::HistoryQueue,
    /// The next draw repaints the whole (alternate) screen.
    full_repaint: bool,
    /// Whether a frame was drawn yet, for the startup trace.
    first_frame_drawn: bool,
    alt_saved_viewport: Option<ratatui::layout::Rect>,
    #[cfg(unix)]
    suspend_context: SuspendContext,
    // True when overlay alt-screen UI is active
    alt_screen_active: Arc<AtomicBool>,
    // True when terminal/tab is focused; updated internally from crossterm events
    terminal_focused: Arc<AtomicBool>,
    enhanced_keys_supported: bool,
}

#[derive(Clone, Debug)]
pub struct FrameRequester {
    frame_schedule_tx: tokio::sync::mpsc::UnboundedSender<Instant>,
}
impl FrameRequester {
    pub fn schedule_frame(&self) {
        let _ = self.frame_schedule_tx.send(Instant::now());
    }
    pub fn schedule_frame_in(&self, dur: Duration) {
        let _ = self.frame_schedule_tx.send(Instant::now() + dur);
    }
}

#[cfg(test)]
impl FrameRequester {
    /// Create a no-op frame requester for tests.
    pub(crate) fn test_dummy() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        FrameRequester {
            frame_schedule_tx: tx,
        }
    }
}

impl Tui {
    pub fn new(terminal: Terminal) -> Self {
        let (frame_schedule_tx, frame_schedule_rx) = tokio::sync::mpsc::unbounded_channel();
        let (draw_tx, _) = tokio::sync::broadcast::channel(1);

        spawn_frame_scheduler(frame_schedule_rx, draw_tx.clone());

        // Detect keyboard enhancement support before any EventStream is created so the
        // crossterm poller can acquire its lock without contention.
        let enhanced_keys_supported = supports_keyboard_enhancement().unwrap_or(false);
        crate::startup_trace::mark("keyboard_query");
        // Cache this to avoid contention with the event reader.
        supports_color::on_cached(supports_color::Stream::Stdout);
        let _ = crate::terminal_palette::default_colors();
        crate::startup_trace::mark("color_query");

        Self {
            frame_schedule_tx,
            draw_tx,
            terminal,
            history: history_queue::HistoryQueue::default(),
            full_repaint: false,
            first_frame_drawn: false,
            alt_saved_viewport: None,
            #[cfg(unix)]
            suspend_context: SuspendContext::new(),
            alt_screen_active: Arc::new(AtomicBool::new(false)),
            terminal_focused: Arc::new(AtomicBool::new(true)),
            enhanced_keys_supported,
        }
    }

    pub fn frame_requester(&self) -> FrameRequester {
        FrameRequester {
            frame_schedule_tx: self.frame_schedule_tx.clone(),
        }
    }

    pub fn enhanced_keys_supported(&self) -> bool {
        self.enhanced_keys_supported
    }

    /// Emit a desktop notification now if the terminal is unfocused.
    /// Returns true if a notification was posted.
    pub fn notify(&mut self, message: impl AsRef<str>) -> bool {
        if !self.terminal_focused.load(Ordering::Relaxed) {
            let _ = execute!(stdout(), PostNotification(message.as_ref().to_string()));
            true
        } else {
            false
        }
    }

    pub fn event_stream(&self) -> Pin<Box<dyn Stream<Item = TuiEvent> + Send + 'static>> {
        use tokio_stream::StreamExt;

        let mut crossterm_events = crossterm::event::EventStream::new();
        let mut draw_rx = self.draw_tx.subscribe();

        // State for tracking how we should resume from ^Z suspend.
        #[cfg(unix)]
        let suspend_context = self.suspend_context.clone();
        #[cfg(unix)]
        let alt_screen_active = self.alt_screen_active.clone();

        let terminal_focused = self.terminal_focused.clone();
        let event_stream = async_stream::stream! {
            loop {
                select! {
                    Some(Ok(event)) = crossterm_events.next() => {
                        match event {
                            Event::Key(key_event) => {
                                #[cfg(unix)]
                                if SUSPEND_KEY.is_press(key_event) {
                                    let _ = suspend_context.suspend(&alt_screen_active);
                                    // We continue here after resume.
                                    yield TuiEvent::Draw;
                                    continue;
                                }
                                yield TuiEvent::Key(key_event);
                            }
                            Event::Resize(_, _) => {
                                yield TuiEvent::Draw;
                            }
                            Event::Paste(pasted) => {
                                yield TuiEvent::Paste(pasted);
                            }
                            Event::Mouse(mouse) => {
                                yield TuiEvent::Mouse(mouse);
                            }
                            Event::FocusGained => {
                                terminal_focused.store(true, Ordering::Relaxed);
                                crate::terminal_palette::requery_default_colors();
                                yield TuiEvent::Draw;
                            }
                            Event::FocusLost => {
                                terminal_focused.store(false, Ordering::Relaxed);
                            }
                        }
                    }
                    result = draw_rx.recv() => {
                        match result {
                            Ok(_) => {
                                yield TuiEvent::Draw;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                // We dropped one or more draw notifications; coalesce to a single draw.
                                yield TuiEvent::Draw;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                // Sender dropped; stop emitting draws from this source.
                            }
                        }
                    }
                }
            }
        };
        Box::pin(event_stream)
    }

    /// Enter alternate screen and expand the viewport to full terminal size, saving the current
    /// inline viewport for restoration when leaving.
    pub fn enter_alt_screen(&mut self) -> Result<()> {
        self.terminal.backend_mut().writer_mut().begin_frame();
        let _ = queue!(self.terminal.backend_mut(), EnterAlternateScreen);
        // Enable "alternate scroll" so terminals may translate wheel to arrows
        let _ = queue!(self.terminal.backend_mut(), EnableAlternateScroll);
        if let Ok(size) = self.terminal.size() {
            self.alt_saved_viewport = Some(self.terminal.viewport_area);
            self.terminal.set_viewport_area(ratatui::layout::Rect::new(
                0,
                0,
                size.width,
                size.height,
            ));
            let _ = self.terminal.clear();
        }
        self.alt_screen_active.store(true, Ordering::Relaxed);
        self.terminal.backend_mut().writer_mut().end_frame()
    }

    /// Report clicks and the wheel as [`TuiEvent::Mouse`] while `capture` is set. The terminal
    /// then selects text only with a modifier (Shift on most), so it is set only for the
    /// transcript overlay.
    pub fn set_mouse_capture(&mut self, capture: bool) -> Result<()> {
        self.terminal.backend_mut().writer_mut().begin_frame();
        let _ = if capture {
            queue!(
                self.terminal.backend_mut(),
                crossterm::event::EnableMouseCapture
            )
        } else {
            queue!(
                self.terminal.backend_mut(),
                crossterm::event::DisableMouseCapture
            )
        };
        self.terminal.backend_mut().writer_mut().end_frame()
    }

    /// Leave alternate screen and restore the previously saved inline viewport, if any.
    pub fn leave_alt_screen(&mut self) -> Result<()> {
        // Disable alternate scroll when leaving alt-screen
        self.terminal.backend_mut().writer_mut().begin_frame();
        let _ = queue!(self.terminal.backend_mut(), DisableAlternateScroll);
        let _ = queue!(self.terminal.backend_mut(), LeaveAlternateScreen);
        if let Some(saved) = self.alt_saved_viewport.take() {
            self.terminal.set_viewport_area(saved);
        }
        self.alt_screen_active.store(false, Ordering::Relaxed);
        self.terminal.backend_mut().writer_mut().end_frame()
    }

    pub fn insert_history_lines(&mut self, lines: Vec<Line<'static>>) {
        self.history.push(lines);
        self.frame_requester().schedule_frame();
    }

    pub(crate) fn clear_pending_history_lines(&mut self) {
        self.history.clear();
    }

    pub(crate) fn is_alt_screen_active(&self) -> bool {
        self.alt_screen_active.load(Ordering::Relaxed)
    }

    pub fn draw(
        &mut self,
        height: u16,
        draw_fn: impl FnOnce(&mut custom_terminal::Frame),
    ) -> Result<()> {
        // If we are resuming from ^Z, we need to prepare the resume action now so we can apply it
        // in the synchronized update.
        #[cfg(unix)]
        let mut prepared_resume = self
            .suspend_context
            .prepare_resume_action(&mut self.terminal, &mut self.alt_saved_viewport);

        // Precompute any viewport updates that need a cursor-position query before entering
        // the synchronized update, to avoid racing with the event reader.
        let mut pending_viewport_area: Option<ratatui::layout::Rect> = None;
        {
            let terminal = &mut self.terminal;
            let screen_size = terminal.size()?;
            let last_known_screen_size = terminal.last_known_screen_size;
            if screen_size != last_known_screen_size
                && let Ok(cursor_pos) = terminal.get_cursor_position()
            {
                let last_known_cursor_pos = terminal.last_known_cursor_pos;
                // If we resized AND the cursor moved, we adjust the viewport area to keep the
                // cursor in the same position. This is a heuristic that seems to work well
                // at least in iTerm2.
                if cursor_pos.y != last_known_cursor_pos.y {
                    let cursor_delta = cursor_pos.y as i32 - last_known_cursor_pos.y as i32;
                    let new_viewport_area = terminal.viewport_area.offset(Offset {
                        x: 0,
                        y: cursor_delta,
                    });
                    pending_viewport_area = Some(new_viewport_area);
                }
            }
        }

        // The whole frame, including the synchronized-update markers, is collected and written
        // with one syscall. The markers are closed and the buffer is sent on the error path too,
        // so a failed frame cannot leave the terminal inside a synchronized update.
        self.terminal.backend_mut().writer_mut().begin_frame();
        let _ = queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate);
        let result = self.draw_frame(
            height,
            draw_fn,
            #[cfg(unix)]
            prepared_resume.take(),
            pending_viewport_area,
        );
        let _ = queue!(self.terminal.backend_mut(), EndSynchronizedUpdate);
        let sent = self.terminal.backend_mut().writer_mut().end_frame();
        if !self.first_frame_drawn {
            self.first_frame_drawn = true;
            crate::startup_trace::mark("first_frame");
        }
        result.and(sent)
    }

    fn draw_frame(
        &mut self,
        height: u16,
        draw_fn: impl FnOnce(&mut custom_terminal::Frame),
        #[cfg(unix)] prepared_resume: Option<job_control::PreparedResumeAction>,
        mut pending_viewport_area: Option<ratatui::layout::Rect>,
    ) -> Result<()> {
        if std::mem::take(&mut self.full_repaint) {
            self.terminal.clear()?;
        }
        #[cfg(unix)]
        if let Some(prepared) = prepared_resume {
            prepared.apply(&mut self.terminal)?;
        }
        let terminal = &mut self.terminal;
        if let Some(new_area) = pending_viewport_area.take() {
            terminal.set_viewport_area(new_area);
            terminal.clear()?;
        }

        if self.history.write_frame(terminal, height)? {
            self.frame_requester().schedule_frame();
        }
        let terminal = &mut self.terminal;

        // Update the y position for suspending so Ctrl-Z can place the cursor correctly.
        #[cfg(unix)]
        {
            let area = terminal.viewport_area;
            let inline_area_bottom = if self.alt_screen_active.load(Ordering::Relaxed) {
                self.alt_saved_viewport
                    .map(|r| r.bottom().saturating_sub(1))
                    .unwrap_or_else(|| area.bottom().saturating_sub(1))
            } else {
                area.bottom().saturating_sub(1)
            };
            self.suspend_context.set_cursor_y(inline_area_bottom);
        }

        terminal.draw(|frame| {
            draw_fn(frame);
        })
    }
}

/// The shortest time between two draws. Requests inside the window are deferred to its end,
/// never dropped, so a burst of events costs one frame instead of one per event.
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(8);

/// When the next draw may fire, given the earliest requested time and when the last one fired.
fn clamp_to_frame_interval(requested: Instant, last_draw: Option<Instant>) -> Instant {
    match last_draw {
        Some(last_draw) => requested.max(last_draw + MIN_FRAME_INTERVAL),
        None => requested,
    }
}

/// Spawn background scheduler to coalesce frame requests and emit draws at deadlines.
fn spawn_frame_scheduler(
    frame_schedule_rx: tokio::sync::mpsc::UnboundedReceiver<Instant>,
    draw_tx: tokio::sync::broadcast::Sender<()>,
) {
    tokio::spawn(async move {
        use tokio::select;
        use tokio::time::Instant as TokioInstant;
        use tokio::time::sleep_until;

        let mut rx = frame_schedule_rx;
        let mut next_deadline: Option<Instant> = None;
        let mut last_draw: Option<Instant> = None;

        loop {
            let target = next_deadline
                .map(|requested| clamp_to_frame_interval(requested, last_draw))
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(60 * 60 * 24 * 365));
            let sleep_fut = sleep_until(TokioInstant::from_std(target));
            tokio::pin!(sleep_fut);

            select! {
                recv = rx.recv() => {
                    match recv {
                        Some(at) => {
                            if next_deadline.is_none_or(|cur| at < cur) {
                                next_deadline = Some(at);
                            }
                            // Do not send a draw immediately here. By continuing the loop,
                            // we recompute the sleep target so the draw fires once via the
                            // sleep branch, coalescing multiple requests into a single draw.
                            continue;
                        }
                        None => break,
                    }
                }
                _ = &mut sleep_fut => {
                    if next_deadline.is_some() {
                        next_deadline = None;
                        last_draw = Some(Instant::now());
                        let _ = draw_tx.send(());
                    }
                }
            }
        }
    });
}

/// Command that emits an OSC 9 desktop notification with a message.
#[derive(Debug, Clone)]
pub struct PostNotification(pub String);

impl Command for PostNotification {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b]9;{}\x07", self.0)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> Result<()> {
        Err(std::io::Error::other(
            "tried to execute PostNotification using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_request_inside_the_window_is_deferred_to_its_end() {
        let last = Instant::now();
        let requested = last + Duration::from_millis(2);
        assert_eq!(
            clamp_to_frame_interval(requested, Some(last)),
            last + MIN_FRAME_INTERVAL
        );
    }

    #[test]
    fn a_request_after_the_window_is_not_delayed() {
        let last = Instant::now();
        let requested = last + Duration::from_millis(50);
        assert_eq!(clamp_to_frame_interval(requested, Some(last)), requested);
    }

    #[test]
    fn the_first_draw_is_not_delayed() {
        let requested = Instant::now();
        assert_eq!(clamp_to_frame_interval(requested, None), requested);
    }

    #[tokio::test]
    async fn a_burst_of_requests_draws_once_per_window() {
        let (frame_tx, frame_rx) = tokio::sync::mpsc::unbounded_channel();
        let (draw_tx, mut draw_rx) = tokio::sync::broadcast::channel(16);
        spawn_frame_scheduler(frame_rx, draw_tx);

        // A request every millisecond.
        let started = Instant::now();
        let mut draws = 0;
        for _ in 0..100 {
            let _ = frame_tx.send(Instant::now());
            tokio::time::sleep(Duration::from_millis(1)).await;
            while draw_rx.try_recv().is_ok() {
                draws += 1;
            }
        }
        // One draw per window at most (the last one may still be pending), and at least one.
        let windows = started.elapsed().as_millis() / MIN_FRAME_INTERVAL.as_millis() + 1;
        assert!(
            draws >= 1 && draws <= windows,
            "{draws} draws in {windows} windows"
        );
    }
}
