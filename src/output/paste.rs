//! The paste fallback (D-011) for text the active layout can't type.
//!
//! A string with any such character is pasted as a whole, not typed in pieces: a paste is
//! asynchronous in the receiving app (it reads the data from a pipe), so keys typed right after
//! one could overtake it and scramble the word. One paste per string also means one clipboard
//! round trip instead of one per run of characters, so fewer chances for a clipboard manager
//! or a slow app to interfere.
//!
//! Flow: save the current selection ([`save`], done before the `sync` round trip that checks
//! the user's modifiers, because reading a large clipboard can take a while), offer the text,
//! press the paste shortcut through EIS, wait until the selection has been read, then offer the
//! saved selection again ([`paste`]).

use std::{future::Future, io, time::Duration};

use tracing::debug;

use super::clipboard::{ClipboardAccess, Offer};

/// How long saving the current selection may take in total.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(3);
/// Pause after offering the text, so clipboard managers that read every new selection right
/// away (Klipper) are done before the paste, and their read isn't taken for the paste.
const SETTLE: Duration = Duration::from_millis(50);
/// How long the app may take to read the pasted text after the shortcut.
const PASTE_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_millis(1500)
};
/// Pause after the read, so the app has inserted the text before anything else happens.
const AFTER_PASTE: Duration = Duration::from_millis(50);

/// What a paste attempt did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The app read the text.
    Pasted,
    /// Nothing was pasted (the reason is for logs); type what the layout can instead.
    Skipped(&'static str),
}

/// The current selection, saved for restoring after a paste. `Err` (a reason for logs) means
/// it can't be restored, so nothing must be pasted.
pub(crate) async fn save(clipboard: &dyn ClipboardAccess) -> Result<Offer, &'static str> {
    match tokio::time::timeout(SNAPSHOT_TIMEOUT, clipboard.snapshot()).await {
        Ok(Ok(previous)) => Ok(previous),
        Ok(Err(e)) => {
            debug!("not pasting: the clipboard can't be saved: {e}");
            Err("the clipboard can't be saved")
        }
        Err(_) => Err("saving the clipboard timed out"),
    }
}

/// Pastes `text` (without control characters) via `clipboard`; `press` sends the paste
/// shortcut. `previous` (from [`save`]) is offered again afterwards, whatever happened. An
/// error from `press` (the EIS connection) is returned as is.
pub(crate) async fn paste<F, Fut>(
    clipboard: &dyn ClipboardAccess,
    previous: Result<Offer, &'static str>,
    text: &str,
    press: F,
) -> io::Result<Outcome>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = io::Result<()>>,
{
    let previous = match previous {
        Ok(previous) => previous,
        Err(why) => return Ok(Outcome::Skipped(why)),
    };
    let mut served = clipboard.served();
    if let Err(e) = clipboard.set(Offer::text(text)).await {
        debug!("not pasting: offering the text failed: {e}");
        restore(clipboard, previous).await;
        return Ok(Outcome::Skipped("offering the text failed"));
    }
    tokio::time::sleep(SETTLE).await;
    let before = *served.borrow_and_update();
    if let Err(e) = press().await {
        restore(clipboard, previous).await;
        return Err(e);
    }
    let read = tokio::time::timeout(PASTE_TIMEOUT, served.wait_for(|n| *n > before))
        .await
        .is_ok_and(|r| r.is_ok());
    if read {
        tokio::time::sleep(AFTER_PASTE).await;
    }
    restore(clipboard, previous).await;
    Ok(if read {
        Outcome::Pasted
    } else {
        Outcome::Skipped("the app didn't read the clipboard")
    })
}

async fn restore(clipboard: &dyn ClipboardAccess, previous: Offer) {
    if let Err(e) = clipboard.set(previous).await {
        debug!("restoring the clipboard failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::output::clipboard::FakeClipboard;

    async fn save_and_paste<F, Fut>(
        clipboard: &dyn ClipboardAccess,
        text: &str,
        press: F,
    ) -> io::Result<Outcome>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = io::Result<()>>,
    {
        let previous = save(clipboard).await;
        paste(clipboard, previous, text, press).await
    }

    /// The app's paste: the shortcut makes it read the selection.
    async fn app_pastes(clipboard: &FakeClipboard, pasted: &std::sync::Mutex<Vec<String>>) {
        if let Some(text) = clipboard.paste() {
            pasted.lock().expect("lock").push(text);
        }
    }

    #[tokio::test]
    async fn pastes_then_restores_the_previous_clipboard() {
        let previous = Offer::text("the user's copy");
        let clipboard = FakeClipboard::new(previous.clone());
        let pasted = std::sync::Mutex::new(Vec::new());
        let outcome = save_and_paste(&*clipboard, "ąžuolas ", || async {
            app_pastes(&clipboard, &pasted).await;
            Ok(())
        })
        .await
        .expect("no EIS error");
        assert_eq!(outcome, Outcome::Pasted);
        assert_eq!(
            pasted.lock().expect("lock").last().map(String::as_str),
            Some("ąžuolas ")
        );
        assert_eq!(clipboard.offer(), previous, "previous clipboard restored");
    }

    #[tokio::test]
    async fn an_unread_paste_is_reported_and_the_clipboard_restored() {
        let previous = Offer::text("keep me");
        let clipboard = FakeClipboard::new(previous.clone());
        let outcome = save_and_paste(&*clipboard, "ą ", || async { Ok(()) })
            .await
            .expect("no EIS error");
        assert!(matches!(outcome, Outcome::Skipped(_)));
        assert_eq!(clipboard.offer(), previous);
    }

    #[tokio::test]
    async fn an_unsaveable_clipboard_is_left_alone() {
        let clipboard = FakeClipboard::new(Offer::text("precious"));
        clipboard.make_unreadable();
        let pressed = AtomicUsize::new(0);
        let outcome = save_and_paste(&*clipboard, "ą ", || async {
            pressed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .await
        .expect("no EIS error");
        assert!(matches!(outcome, Outcome::Skipped(_)));
        assert_eq!(pressed.load(Ordering::Relaxed), 0, "no paste shortcut");
        assert!(clipboard.sets().is_empty(), "clipboard untouched");
    }

    #[tokio::test]
    async fn an_empty_clipboard_is_restored_as_empty() {
        let clipboard = FakeClipboard::new(Offer::default());
        let outcome = save_and_paste(&*clipboard, "ą ", || async {
            clipboard.paste();
            Ok(())
        })
        .await
        .expect("no EIS error");
        assert_eq!(outcome, Outcome::Pasted);
        assert!(clipboard.offer().is_empty());
        assert_eq!(
            clipboard.sets().len(),
            2,
            "text offered, then the empty clipboard"
        );
    }

    #[tokio::test]
    async fn a_clipboard_manager_reading_the_offer_is_not_taken_for_the_paste() {
        let clipboard = FakeClipboard::new(Offer::default());
        // The manager reads the offer before the shortcut; the app then ignores the shortcut.
        clipboard.add_manager();
        let outcome = save_and_paste(&*clipboard, "ą ", || async { Ok(()) })
            .await
            .expect("no EIS error");
        assert!(matches!(outcome, Outcome::Skipped(_)), "{outcome:?}");
    }

    #[tokio::test]
    async fn an_eis_error_is_returned_after_restoring() {
        let previous = Offer::text("keep me");
        let clipboard = FakeClipboard::new(previous.clone());
        let result = save_and_paste(&*clipboard, "ą ", || async {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(clipboard.offer(), previous);
    }
}
