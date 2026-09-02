//! Progress, as events rather than prints.
//!
//! Fitting a pile is slow in exactly the way that needs saying out loud: a
//! whole-file type verification per member, and sometimes a network round
//! trip that costs money. The CLI could get away with `eprintln!` — one
//! process, one terminal, stderr right there. A TUI cannot (it owns the
//! screen), an MCP server must not (stdout is protocol), and neither can
//! show a spinner for work that reports nothing until it is finished.
//!
//! So the work emits [`Event`]s and the caller decides what they mean. The
//! sink is owned rather than borrowed, because the TUI runs the fit on a
//! spawned task and a borrowed sink would not survive the `'static` bound.

use std::sync::Arc;

/// Something worth telling a user while a pile is being fitted.
#[derive(Debug, Clone)]
pub enum Event {
    /// Planning this member is about to start. Sent before any I/O, so a UI
    /// can show which file it is waiting on rather than a bare spinner.
    MemberStarted { path: String, index: usize, total: usize },
    /// Planning this member finished, one way or another.
    MemberFinished {
        path: String,
        index: usize,
        total: usize,
        status: crate::report::MemberStatus,
    },
    /// A model is about to be asked for a frame.
    ///
    /// Its own event because it is the one step that leaves the machine and
    /// spends money: a UI should be able to say so *while it happens*, not
    /// afterwards in a note.
    Consulting { path: String, backend: String, model: String, bytes: u64 },
    /// A remark for the user that is not tied to a member — e.g. a
    /// low-confidence spec warning during a query's pre-pass.
    Note(String),
}

/// Where events go. `Send + Sync` so a fit can run on a spawned task.
pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// Send an event, if anyone is listening.
pub(crate) fn emit(sink: Option<&Sink>, event: Event) {
    if let Some(s) = sink {
        s(event);
    }
}

/// The CLI's sink: the one event a terminal user needs unprompted is that
/// their file is being sent to a model. Per-member progress stays silent
/// there because the CLI prints the whole table when it is done.
pub fn stderr_sink() -> Sink {
    Arc::new(|e| match e {
        Event::Consulting { path, backend, model, bytes } => {
            eprintln!(
                "note: sending {bytes} bytes sampled from {path} to {backend} ({model}) \
                 to propose a frame"
            );
        }
        Event::Note(t) => eprintln!("{t}"),
        Event::MemberStarted { .. } | Event::MemberFinished { .. } => {}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the CLI sink must not panic on a `Note` event, the one
    /// this module adds for warnings that used to be printed directly by
    /// their callers.
    #[test]
    fn stderr_sink_handles_note() {
        (stderr_sink())(Event::Note("x".into()));
    }
}
