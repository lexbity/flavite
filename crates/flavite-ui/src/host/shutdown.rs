//! Quit schedule (RFC-0003 W12, D-10/D-12): the window close request runs
//! through document decisions, accepted-operation flushes, and the layout
//! flush before the window may exit. A timeout can never turn a pending
//! save into `Saved`: every label is truthful, and quitting with unresolved
//! work requires a separate explicit consequential choice.

use crate::app::{App, Message};
use crate::host::dialogs::Dialog;
use flavite_workspace::TabId;
use iced::Task;
use std::time::{Duration, Instant};

/// The bounded wait for accepted operations to reach terminal state
/// before the user is offered explicit consequential choices.
pub const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// The quit schedule's stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownStage {
    /// Note decisions are being resolved through the close plans.
    Documents,
    /// Accepted operations are draining toward terminal state.
    AwaitingOperations { started: Instant },
    /// Everything safe is durable; the window may exit.
    ReadyToExit,
}

/// Messages driving the quit schedule.
#[derive(Debug, Clone)]
pub enum ShutdownMessage {
    /// Continue the schedule (pump-driven).
    Step,
    /// Keep waiting for in-flight operations past the timeout.
    KeepWaiting,
    /// The explicit consequential choice: exit despite unresolved work.
    QuitAnyway,
    /// Cancel the quit; the workspace stays exactly as it is now.
    Cancel,
}

/// The active quit schedule.
#[derive(Debug, Clone)]
pub struct ShutdownPlan {
    /// The current stage.
    pub stage: ShutdownStage,
}

impl App {
    /// The window close request: begin the quit schedule (W12).
    pub fn begin_shutdown(&mut self) {
        if self.has_composition() {
            self.status = "Finish or cancel text composition before quitting".into();
            return;
        }
        if self.dialog.is_some() {
            self.status = "Resolve the open dialog before quitting".into();
            return;
        }
        // Every note tab that would need a decision queues through the
        // close machinery (origin Quit); clean/shared tabs close directly.
        let tabs: Vec<TabId> = self
            .workspace
            .tabs()
            .filter(|(_, view)| {
                matches!(
                    view.kind(),
                    flavite_workspace::ViewKind::Note(_) | flavite_workspace::ViewKind::History(_)
                )
            })
            .map(|(tab, _)| *tab)
            .collect();
        let mut queue = Vec::new();
        for tab in tabs {
            if self.close_needs_decision(tab) {
                queue.push(tab);
            } else {
                let _ = self.complete_safe_close(tab, crate::host::closing::CloseOrigin::Quit);
            }
        }
        self.shutdown = Some(crate::host::shutdown::ShutdownPlan {
            stage: ShutdownStage::Documents,
        });
        self.close_queue = queue;
        // advance_close_queue starts the operation drain when nothing needs
        // a decision, and continues it after the last decision resolves.
        self.advance_close_queue();
        if self.closing.is_some() {
            self.status = "Quitting: resolve the unsaved-changes decisions to continue".into();
        }
    }

    /// All document decisions are resolved: wait for accepted operations,
    /// then flush the layout.
    pub fn begin_operation_drain(&mut self) {
        if let Some(shutdown) = self.shutdown.as_mut() {
            shutdown.stage = ShutdownStage::AwaitingOperations {
                started: Instant::now(),
            };
        }
        let _ = self.shutdown_step();
    }

    /// Advance the schedule after each pump. Returns the exit task when
    /// the window may close.
    pub fn shutdown_step(&mut self) -> Option<Task<Message>> {
        let stage = self.shutdown.as_ref()?.stage;
        match stage {
            ShutdownStage::ReadyToExit => {
                self.shutdown = None;
                Some(exit_task())
            }
            ShutdownStage::Documents => None,
            ShutdownStage::AwaitingOperations { started } => {
                let pending = self.pending_operations();
                if pending == 0 {
                    // All accepted work reached terminal state; flush the
                    // layout durably and exit.
                    self.save_layout_now();
                    if self.persistence.last_error().is_some() {
                        // Layout durability failed: the user may explicitly
                        // quit with the unsaved layout; notes are already
                        // safe (D-12: independent durability).
                        self.dialog = Some(Dialog::ShutdownTimeout {
                            concern: "The workspace layout could not be saved. Notes are safe. \
                                      Quitting now loses the latest layout changes."
                                .into(),
                        });
                    } else {
                        self.shutdown = Some(ShutdownPlan {
                            stage: ShutdownStage::ReadyToExit,
                        });
                    }
                } else if started.elapsed() >= SHUTDOWN_DRAIN_TIMEOUT {
                    // The timeout never mislabels pending work as saved:
                    // the user gets the typed, consequential choice.
                    self.dialog = Some(Dialog::ShutdownTimeout {
                        concern: format!(
                            "{pending} accepted operation(s) are still in flight. Quitting now \
                             may lose unsaved changes."
                        ),
                    });
                }
                None
            }
        }
    }

    /// Reduce a shutdown message.
    pub fn update_shutdown(&mut self, message: ShutdownMessage) -> Task<Message> {
        match message {
            ShutdownMessage::Step => Task::none(),
            ShutdownMessage::KeepWaiting => {
                self.dialog = None;
                if let Some(shutdown) = self.shutdown.as_mut() {
                    shutdown.stage = ShutdownStage::AwaitingOperations {
                        started: Instant::now(),
                    };
                }
                self.status = "Continuing to wait for in-flight operations".into();
                Task::none()
            }
            ShutdownMessage::QuitAnyway => {
                // The explicit consequential choice. Note/recovery failures
                // are never silently absorbed: the concern text named them,
                // and this exit path skips no flush that already succeeded.
                self.dialog = None;
                self.status = "Quitting with unresolved work (explicit choice)".into();
                if let Some(shutdown) = self.shutdown.as_mut() {
                    shutdown.stage = ShutdownStage::ReadyToExit;
                }
                Task::none()
            }
            ShutdownMessage::Cancel => {
                self.cancel_shutdown();
                Task::none()
            }
        }
    }

    /// Cancel the quit; the workspace returns to its actual current state
    /// (never a snapshot from before the schedule started).
    pub fn cancel_shutdown(&mut self) {
        self.dialog = None;
        self.shutdown = None;
        self.close_queue.clear();
        self.closing = None;
        self.status = "Quit cancelled; the workspace is unchanged".into();
    }
}

/// Close the shell's window: the oldest (and only) window.
pub fn exit_task() -> Task<Message> {
    iced::window::oldest().and_then(iced::window::close::<Message>)
}
