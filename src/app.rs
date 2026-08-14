//! Ratatui application loop and dirty-driven rendering.

use std::io;
use std::time::Duration;

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use std::time::Instant;

use crate::controller::Controller;
use crate::host::LaunchContext;
use crate::input::{InputMode, map_event_with_keybindings};
use crate::model::AppModel;
use crate::worker::WorkerRuntime;

const WORKER_WAIT_INTERVAL: Duration = Duration::from_millis(50);

pub struct App {
    controller: Controller,
    workers: WorkerRuntime,
    dirty: bool,
}

impl App {
    #[must_use]
    pub fn new(context: LaunchContext) -> Self {
        Self {
            controller: Controller::new(context),
            workers: WorkerRuntime::new(),
            dirty: true,
        }
    }

    pub fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> io::Result<()> {
        let _mouse_capture = MouseCapture::enable()?;

        // The shell is deliberately drawn before project resolution or any other blocking work.
        terminal.draw(|frame| self.render(frame))?;
        self.controller.start(&mut self.workers);

        loop {
            let pending = self.has_pending_work();
            let refresh_wait = self.controller.next_refresh_in(Instant::now());
            let wait = match (pending, refresh_wait) {
                (true, Some(refresh_wait)) => Some(WORKER_WAIT_INTERVAL.min(refresh_wait)),
                (true, None) => Some(WORKER_WAIT_INTERVAL),
                (false, refresh_wait) => refresh_wait,
            };
            let input = if let Some(wait) = wait {
                event::poll(wait)?.then(event::read).transpose()?
            } else {
                Some(event::read()?)
            };

            let quit = if let Some(input) = input
                && let Some(intent) = map_event_with_keybindings(
                    input,
                    InputMode::Normal,
                    self.controller.model().geometry(),
                    Some(self.controller.keybindings()),
                ) {
                let transition = self.controller.apply(intent, &mut self.workers);
                self.dirty |= transition.dirty;
                transition.quit
            } else {
                false
            };

            while let Some(result) = self.workers.try_recv() {
                self.dirty |= self.controller.apply_result(result, &mut self.workers);
            }
            self.dirty |= self.controller.tick(Instant::now(), &mut self.workers);

            if quit {
                break;
            }
            if self.dirty {
                terminal.draw(|frame| self.render(frame))?;
            }
        }

        self.workers.shutdown();
        Ok(())
    }

    pub fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        self.controller.render(frame.area(), frame.buffer_mut());
        self.dirty = false;
    }

    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[must_use]
    pub fn has_pending_work(&self) -> bool {
        self.workers.has_pending_work() || self.controller.files_have_pending_work()
    }

    #[must_use]
    pub const fn model(&self) -> &AppModel {
        self.controller.model()
    }

    pub const fn model_mut(&mut self) -> &mut AppModel {
        self.controller.model_mut()
    }
}

struct MouseCapture;

impl MouseCapture {
    fn enable() -> io::Result<Self> {
        execute!(io::stdout(), EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
}
