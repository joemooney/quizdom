// trace:STORY-83 | ai:claude
//! A small ascii "thinking" spinner for blocking LLM calls.
//!
//! Blocking LLM calls (next-question generation; also honing / contradiction)
//! take seconds with no feedback, so the CLI looks frozen. [`Spinner::start`]
//! animates a one-line indicator on a **background thread**, writing to
//! **stderr** so stdout / piped output stays byte-for-byte unchanged. It is
//! active **only when stderr is a TTY** — under test, in pipes, or in
//! redirects it returns an inert handle that spawns no thread and writes
//! nothing. Dropping the handle stops the thread and clears the spinner line
//! before the next output lands.
//!
//! Wrap a blocking call by holding the guard for the call's duration:
//!
//! ```ignore
//! let result = {
//!     let _spinner = Spinner::start("thinking");
//!     runtime.block_on(client.call(/* ... */))
//! }; // guard drops here -> thread stops, line cleared
//! ```

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The animation frames cycled on the spinner line — a classic ascii spin so
/// no terminal needs unicode or a particular font.
const FRAMES: &[&str] = &["|", "/", "-", "\\"];

/// How long each frame is shown.
const INTERVAL: Duration = Duration::from_millis(120);

/// A running spinner. While alive, a background thread repaints the spinner
/// line on stderr; dropping it stops the thread and clears the line. An inert
/// spinner (non-TTY stderr) holds no thread and does nothing on drop.
pub(crate) struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    /// Start a spinner with `label` iff stderr is a terminal; otherwise return
    /// an inert handle (no thread, no output) so tests / pipes / redirects are
    /// unaffected.
    pub(crate) fn start(label: &str) -> Self {
        Self::start_if(std::io::stderr().is_terminal(), label)
    }

    /// Core of [`start`](Self::start) with the TTY decision passed in, so the
    /// gating is testable without a real terminal. When `active` is false no
    /// thread is spawned and the handle is inert.
    fn start_if(active: bool, label: &str) -> Self {
        if !active {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let label = label.to_string();
        let handle = thread::spawn(move || {
            let mut err = std::io::stderr();
            let mut frame = 0usize;
            while !thread_stop.load(Ordering::Relaxed) {
                let _ = write!(err, "{}", frame_line(frame, &label));
                let _ = err.flush();
                frame = frame.wrapping_add(1);
                thread::sleep(INTERVAL);
            }
            // Clear the spinner line so the next output starts clean.
            let _ = write!(err, "{}", clear_line(&label));
            let _ = err.flush();
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The bytes painted for one animation frame: carriage return, the spinning
/// glyph, a space, and the label. Returning to column zero each tick lets the
/// next frame (or [`clear_line`]) overwrite it in place.
fn frame_line(frame: usize, label: &str) -> String {
    format!("\r{} {}", FRAMES[frame % FRAMES.len()], label)
}

/// The bytes that erase the spinner line: a carriage return, enough spaces to
/// cover the widest frame (`glyph + space + label`), then a carriage return so
/// the cursor rests at column zero for the next write.
fn clear_line(label: &str) -> String {
    let width = 2 + label.chars().count();
    format!("\r{}\r", " ".repeat(width))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_line_cycles_through_every_frame() {
        assert_eq!(frame_line(0, "thinking"), "\r| thinking");
        assert_eq!(frame_line(1, "thinking"), "\r/ thinking");
        assert_eq!(frame_line(2, "thinking"), "\r- thinking");
        assert_eq!(frame_line(3, "thinking"), "\r\\ thinking");
        // Wraps around after the last frame.
        assert_eq!(frame_line(4, "thinking"), frame_line(0, "thinking"));
    }

    #[test]
    fn clear_line_covers_glyph_space_and_label() {
        // "| thinking" is 10 columns, so the clear must blank at least 10.
        let cleared = clear_line("thinking");
        assert_eq!(cleared, "\r          \r");
        assert!(cleared.starts_with('\r') && cleared.ends_with('\r'));
        assert_eq!(cleared.matches(' ').count(), 2 + "thinking".len());
    }

    #[test]
    fn inert_spinner_spawns_no_thread() {
        // Non-TTY stderr -> inert handle that does nothing on drop.
        let spinner = Spinner::start_if(false, "thinking");
        assert!(spinner.handle.is_none(), "no background thread when inert");
        assert!(spinner.stop.load(Ordering::Relaxed));
        drop(spinner); // must not panic or block
    }

    #[test]
    fn active_spinner_runs_and_stops_cleanly() {
        // We can't assert TTY output here, but starting and dropping an active
        // spinner must spawn a joinable thread and tear down without hanging.
        let spinner = Spinner::start_if(true, "thinking");
        assert!(spinner.handle.is_some(), "active spinner holds a thread");
        drop(spinner); // joins the thread; should return promptly
    }

    // trace:BUG-100 | ai:claude
    #[test]
    fn inert_spinner_held_across_a_multi_step_region_emits_nothing() {
        // BUG-100 widens the guarded region from just the LLM call to the whole
        // next-question computation (candidate gather + LLM + persist). When
        // stderr is not a TTY the spinner must stay inert no matter how many
        // sub-steps it spans, so piped / redirected output is byte-for-byte
        // unchanged. The guard owns no thread across the entire region.
        let spinner = Spinner::start_if(false, "thinking");
        assert!(spinner.handle.is_none(), "inert across the whole region");
        // Simulate the wider scope's sub-steps; the guard outlives all of them.
        for _step in 0..3 {
            assert!(
                spinner.stop.load(Ordering::Relaxed),
                "inert guard never animates between sub-steps",
            );
        }
        drop(spinner); // dropping after a long region must not panic or block
    }
}
