//! Background cleanup of finished sentences while the user is still talking, so
//! key-up only cleans the last sentence or two. Decision logic lives here as
//! pure functions over strings plus a cleaner closure; `actions.rs` wires the
//! real LLM call, the settings and the overlay.
//!
//! One sentence of look-back: the last complete sentence is never cleaned in
//! the background, so a correction cue in it ("scratch that", "actually") can
//! still reach the sentence before it. A cue spanning 2+ sentences across a
//! boundary is a known limit.

use log::warn;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{watch, Notify};

/// How often the background loop re-checks for a cancel while idle.
const IDLE_POLL: Duration = Duration::from_millis(250);

/// Work finished in the background so far.
#[derive(Default, Clone, Debug)]
pub struct Cleaned {
    /// Cleaned chunks, joined by a space.
    pub text: String,
    /// The exact raw text those chunks came from (a prefix of the raw stream).
    pub covered_raw: String,
    /// LLM calls made during speech.
    pub calls: usize,
    /// Chunks used as is because `needs_llm_cleanup` said they were clean.
    pub skipped: usize,
}

/// Stop signal for the background loop: no new LLM call starts once set.
#[derive(Default)]
pub struct Stop {
    flag: AtomicBool,
    notify: Notify,
}

impl Stop {
    pub fn set(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

/// Byte ranges of the complete sentences in `s`. A terminator (. ? !) ends a
/// sentence only when whitespace or the end of the text follows, so "3.5" and
/// "e.g.x" do not split. Leading whitespace belongs to the range.
fn complete_sentences(s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if matches!(c, '.' | '?' | '!') {
            let end = i + c.len_utf8();
            if s[end..].chars().next().is_none_or(char::is_whitespace) {
                out.push((start, end));
                start = end;
            }
        }
    }
    out
}

/// The raw text after `covered_raw` that is safe to clean now: every complete
/// sentence except the last complete one. `None` when `raw` no longer starts
/// with `covered_raw` or fewer than two complete sentences are waiting.
pub fn next_chunk<'a>(raw: &'a str, covered_raw: &str) -> Option<&'a str> {
    let rest = raw.strip_prefix(covered_raw)?;
    let sentences = complete_sentences(rest);
    let keep_back = sentences.len().checked_sub(2)?;
    Some(&rest[..sentences[keep_back].1])
}

fn push_piece(out: &mut String, piece: &str) {
    let piece = piece.trim();
    if piece.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(piece);
}

/// Cleans sentences as they finish. `clean` returns `None` when the LLM call
/// failed or timed out; that chunk is then kept raw (`[FALLBACK]`). Runs until
/// stopped, cancelled, or the raw-text channel closes; at most one `clean`
/// call is in flight, and none starts after `stop` is set.
pub async fn run_background<C, Fut>(
    mut raw_rx: watch::Receiver<String>,
    stop: Arc<Stop>,
    state: Arc<Mutex<Cleaned>>,
    is_cancelled: impl Fn() -> bool,
    clean: C,
) where
    C: Fn(String) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    loop {
        if stop.is_set() || is_cancelled() {
            return;
        }
        let raw = raw_rx.borrow_and_update().clone();
        let chunk = {
            let st = state.lock().unwrap();
            next_chunk(&raw, &st.covered_raw).map(str::to_string)
        };
        let Some(chunk) = chunk else {
            tokio::select! {
                changed = raw_rx.changed() => if changed.is_err() { return },
                _ = stop.notify.notified() => {}
                _ = tokio::time::sleep(IDLE_POLL) => {}
            }
            continue;
        };

        let trimmed = chunk.trim();
        let (text, called) = if crate::audio_toolkit::needs_llm_cleanup(trimmed) {
            match clean(trimmed.to_string()).await {
                Some(c) if !c.trim().is_empty() => (c, true),
                _ => {
                    warn!("[FALLBACK] Background cleanup failed; keeping the raw sentence(s)");
                    (trimmed.to_string(), true)
                }
            }
        } else {
            (trimmed.to_string(), false)
        };

        let mut st = state.lock().unwrap();
        push_piece(&mut st.text, &text);
        st.covered_raw.push_str(&chunk);
        if called {
            st.calls += 1;
        } else {
            st.skipped += 1;
        }
    }
}

/// What key-up still has to clean.
#[derive(Debug, PartialEq)]
pub enum Plan<'a> {
    /// Nothing usable from the background: clean the whole final text as before.
    Whole,
    /// Paste `prefix` (already cleaned) and clean only `remaining`.
    Tail { prefix: &'a str, remaining: &'a str },
}

/// The final transcript may differ from the text streamed during speech, so
/// the background work counts only when the final text still starts with
/// exactly the raw text it covered.
pub fn plan<'a>(final_text: &'a str, bg: &'a Cleaned) -> Plan<'a> {
    if bg.covered_raw.is_empty() {
        return Plan::Whole;
    }
    match final_text.strip_prefix(bg.covered_raw.as_str()) {
        Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => Plan::Tail {
            prefix: bg.text.as_str(),
            remaining: rest.trim(),
        },
        _ => Plan::Whole,
    }
}

pub fn join_cleaned(prefix: &str, tail: &str) -> String {
    let mut out = String::new();
    push_piece(&mut out, prefix);
    push_piece(&mut out, tail);
    out
}

/// Result of the key-up step when the background work was usable.
#[derive(Debug, PartialEq)]
pub struct Tailed {
    pub text: String,
    pub tail_words: usize,
}

/// Key-up: clean only what the background left, then join. `None` means the
/// background work is unusable and the caller cleans the whole text. `clean`
/// runs only when the tail may need it and returns `None` on failure or
/// timeout, which pastes the raw tail.
pub async fn finish_take<C, Fut>(final_text: &str, bg: &Cleaned, clean: C) -> Option<Tailed>
where
    C: FnOnce(String) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    let Plan::Tail { prefix, remaining } = plan(final_text, bg) else {
        return None;
    };
    let cleaned_tail =
        if remaining.is_empty() || !crate::audio_toolkit::needs_llm_cleanup(remaining) {
            remaining.to_string()
        } else {
            clean(remaining.to_string())
                .await
                .unwrap_or_else(|| remaining.to_string())
        };
    Some(Tailed {
        text: join_cleaned(prefix, &cleaned_tail),
        tail_words: remaining.split_whitespace().count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn bg(text: &str, covered_raw: &str) -> Cleaned {
        Cleaned {
            text: text.into(),
            covered_raw: covered_raw.into(),
            ..Default::default()
        }
    }

    #[test]
    fn last_sentence_is_kept_back_for_a_correction_cue() {
        // The cue sentence stays raw; only the sentence before it is cleaned.
        assert_eq!(
            next_chunk("Send it Monday. Scratch that, Tuesday.", ""),
            Some("Send it Monday.")
        );
        // One complete sentence: nothing may be cleaned yet.
        assert_eq!(next_chunk("Scratch that, Tuesday.", ""), None);
        // A trailing unfinished fragment does not count as a later sentence.
        assert_eq!(next_chunk("Send it Monday. Scratch that", ""), None);
        // Three sentences: the first two go, the last stays.
        assert_eq!(next_chunk("One. Two! Three?", ""), Some("One. Two!"));
    }

    #[test]
    fn chunks_continue_after_the_covered_text() {
        let raw = "One. Two. Three. Four.";
        assert_eq!(next_chunk(raw, "One. Two."), Some(" Three."));
        // The covered text no longer matches: nothing to do.
        assert_eq!(next_chunk("Uno. Two. Three.", "One."), None);
    }

    #[test]
    fn decimals_and_abbreviations_do_not_split() {
        assert_eq!(
            next_chunk("It costs 3.5 rupees. Ok. Yes.", ""),
            Some("It costs 3.5 rupees. Ok.")
        );
    }

    #[test]
    fn matching_final_text_cleans_only_the_tail() {
        let b = bg("Send it Monday.", "Send it Monday.");
        assert_eq!(
            plan("Send it Monday. Scratch that, Tuesday.", &b),
            Plan::Tail {
                prefix: "Send it Monday.",
                remaining: "Scratch that, Tuesday."
            }
        );
    }

    #[test]
    fn mismatched_final_text_discards_the_background_work() {
        let b = bg("Send it Monday.", "Send it Monday.");
        assert_eq!(plan("Send it on Monday. Scratch that.", &b), Plan::Whole);
        // Shares the leading letters but diverges mid-word.
        assert_eq!(plan("Send it Monday.5 more", &b), Plan::Whole);
        // Nothing was cleaned in the background.
        assert_eq!(plan("Anything at all.", &Cleaned::default()), Plan::Whole);
        let called = AtomicUsize::new(0);
        let out = tauri::async_runtime::block_on(finish_take(
            "Send it on Monday. Scratch that.",
            &b,
            |_| async {
                called.fetch_add(1, Ordering::SeqCst);
                Some("x".to_string())
            },
        ));
        assert_eq!(out, None);
        assert_eq!(called.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn output_is_cleaned_prefix_then_cleaned_tail() {
        let b = bg("Send it Monday.", "Send it Monday.");
        let out = tauri::async_runtime::block_on(finish_take(
            "Send it Monday. Scratch that, Tuesday.",
            &b,
            |rem| async move {
                assert_eq!(rem, "Scratch that, Tuesday.");
                Some("Send it Tuesday.".to_string())
            },
        ))
        .unwrap();
        assert_eq!(out.text, "Send it Monday. Send it Tuesday.");
        assert_eq!(out.tail_words, 3);
        assert_eq!(join_cleaned("A.", "B."), "A. B.");
        assert_eq!(join_cleaned("A.", ""), "A.");
    }

    #[test]
    fn tail_falls_back_to_raw_when_cleanup_fails_or_is_not_needed() {
        let b = bg("Done.", "Done.");
        let failed = tauri::async_runtime::block_on(finish_take(
            "Done. Scratch that, Friday.",
            &b,
            |_| async { None },
        ))
        .unwrap();
        assert_eq!(failed.text, "Done. Scratch that, Friday.");
        // A clean tail never reaches the LLM.
        let clean =
            tauri::async_runtime::block_on(finish_take("Done. All good here.", &b, |_| async {
                panic!("clean tail must not call the LLM")
            }))
            .unwrap();
        assert_eq!(clean.text, "Done. All good here.");
    }

    /// Drives `run_background` with a fake cleaner on a raw-text channel.
    fn run_fake(raw: &'static str, stop_before: bool, calls: Arc<AtomicUsize>) -> Cleaned {
        tauri::async_runtime::block_on(async move {
            let (tx, rx) = watch::channel(String::new());
            let stop = Arc::new(Stop::default());
            let state = Arc::new(Mutex::new(Cleaned::default()));
            if stop_before {
                stop.set();
            }
            let task = tokio::spawn(run_background(
                rx,
                Arc::clone(&stop),
                Arc::clone(&state),
                || false,
                move |chunk| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Some(format!("[{chunk}]"))
                    }
                },
            ));
            tx.send_replace(raw.to_string());
            tokio::time::sleep(Duration::from_millis(100)).await;
            stop.set();
            task.await.unwrap();
            let out = state.lock().unwrap().clone();
            out
        })
    }

    #[test]
    fn background_cleans_sentences_but_keeps_the_last_back() {
        let calls = Arc::new(AtomicUsize::new(0));
        let out = run_fake(
            "Send it Monday. No wait, Tuesday. Then call me.",
            false,
            Arc::clone(&calls),
        );
        // Both earlier sentences carry cues: one call, one chunk, last kept back.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(out.text, "[Send it Monday. No wait, Tuesday.]");
        assert_eq!(out.covered_raw, "Send it Monday. No wait, Tuesday.");
        assert_eq!((out.calls, out.skipped), (1, 0));
    }

    #[test]
    fn clean_sentences_skip_the_llm() {
        let calls = Arc::new(AtomicUsize::new(0));
        let out = run_fake(
            "We ship today. The team agrees. Fine.",
            false,
            Arc::clone(&calls),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(out.text, "We ship today. The team agrees.");
        assert_eq!((out.calls, out.skipped), (0, 1));
    }

    #[test]
    fn no_new_call_starts_after_key_up() {
        let calls = Arc::new(AtomicUsize::new(0));
        let out = run_fake(
            "Send it Monday. No wait, Tuesday. Then call me.",
            true,
            Arc::clone(&calls),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(out.covered_raw, "");
    }

    #[test]
    fn in_flight_call_finishes_after_stop_and_no_second_one_starts() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_task = Arc::clone(&calls);
        let out = tauri::async_runtime::block_on(async move {
            let (tx, rx) = watch::channel(String::new());
            let stop = Arc::new(Stop::default());
            let state = Arc::new(Mutex::new(Cleaned::default()));
            let stop_in_call = Arc::clone(&stop);
            let task = tokio::spawn(run_background(
                rx,
                Arc::clone(&stop),
                Arc::clone(&state),
                || false,
                move |chunk| {
                    let calls = Arc::clone(&calls_task);
                    let stop = Arc::clone(&stop_in_call);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        // Key-up lands while this call is in flight.
                        stop.set();
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Some(format!("[{chunk}]"))
                    }
                },
            ));
            tx.send_replace("A no wait. B no wait. C no wait. D.".to_string());
            task.await.unwrap();
            let out = state.lock().unwrap().clone();
            out
        });
        // The in-flight call (the first chunk, all of A-C) completed; nothing after.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(out.text, "[A no wait. B no wait. C no wait.]");
    }

    #[test]
    fn cancel_stops_the_loop_without_calling() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_task = Arc::clone(&calls);
        tauri::async_runtime::block_on(async move {
            let (tx, rx) = watch::channel("A no wait. B no wait. C.".to_string());
            let state = Arc::new(Mutex::new(Cleaned::default()));
            run_background(
                rx,
                Arc::new(Stop::default()),
                state,
                || true,
                move |_| {
                    let calls = Arc::clone(&calls_task);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        None
                    }
                },
            )
            .await;
            drop(tx);
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
