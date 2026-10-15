//! Learn words from edits: shortly after a paste, re-read the text box. When a
//! pasted word was changed to a close spelling that looks like a name or
//! jargon, save that spelling to the custom words so the next dictation gets
//! it right.

use crate::context_words::read_focused_text;
use crate::settings::{get_settings, write_settings};
use std::time::Duration;
use tauri::AppHandle;

/// When to re-read the text box, counted from the paste.
const CHECK_AFTER: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];
/// A fixed spelling must be at least this close to the pasted word (0 to 1).
const MIN_SIMILARITY: f64 = 0.5;

/// Re-reads the focused text box after a paste and saves corrected spellings.
pub fn spawn_learn(app: AppHandle, pasted: String) {
    std::thread::spawn(move || {
        let mut waited = Duration::ZERO;
        for at in CHECK_AFTER {
            std::thread::sleep(at - waited);
            waited = at;
            let learned = corrected_words(&pasted, &read_focused_text());
            let mut settings = get_settings(&app);
            let added = add_custom_words(&mut settings, &learned);
            if added > 0 {
                log::info!("Learned {} word(s) from an edit", added);
                write_settings(&app, settings);
                // The settings page keeps its own copy of the list and saves it whole when a word is
                // added by hand; without a refresh that save would delete the words learned here.
                let _ = app.emit(
                    "settings-changed",
                    serde_json::json!({ "setting": "custom_words" }),
                );
            }
        }
    });
}

fn tokens(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .flat_map(split_sentences)
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|t| !t.is_empty())
        .collect()
}

/// Splits "lunch.Ask" into "lunch." and "Ask": two dictations pasted back to
/// back with no space between them are two words, not a new name to learn.
/// A split needs lower case before the stop and "Xy" after it, so names such
/// as "ASP.NET", "Node.JS" or "llama.cpp" stay whole.
fn split_sentences(token: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = token.char_indices().collect();
    let mut parts = Vec::new();
    let mut start = 0;
    for w in chars.windows(4) {
        let [(_, before), (_, stop), (at, upper), (_, lower)] = [w[0], w[1], w[2], w[3]];
        if before.is_lowercase()
            && matches!(stop, '.' | '!' | '?')
            && upper.is_uppercase()
            && lower.is_lowercase()
        {
            parts.push(&token[start..at]);
            start = at;
        }
    }
    parts.push(&token[start..]);
    parts
}

/// Spellings typed over pasted words. For each pasted word no longer in the
/// box, the closest new word in the box is learned when it looks like a name
/// or jargon: a spelling fix needs a capital or a digit, and a fix that only
/// changes case needs a capital after the first letter or a digit, so
/// capitalising a sentence start ("the" to "The") is never learned. Nothing is
/// learned when the box no longer holds most of the pasted text.
pub fn corrected_words(pasted: &str, now: &str) -> Vec<String> {
    let pasted_tokens = tokens(pasted);
    let now_tokens = tokens(now);
    let kept = pasted_tokens
        .iter()
        .filter(|p| now_tokens.contains(p))
        .count();
    if pasted_tokens.is_empty() || kept * 2 < pasted_tokens.len() {
        return Vec::new();
    }
    let new_tokens: Vec<&str> = now_tokens
        .iter()
        .copied()
        .filter(|n| !pasted_tokens.contains(n))
        .collect();
    let starts = sentence_starts(now);
    let mut learned: Vec<String> = Vec::new();
    for missing in pasted_tokens.iter().filter(|p| !now_tokens.contains(p)) {
        let best = new_tokens
            .iter()
            .filter(|n| n.chars().count() >= 3 && looks_like_a_name(missing, n, starts.contains(n)))
            .map(|n| {
                let score =
                    strsim::normalized_levenshtein(&missing.to_lowercase(), &n.to_lowercase());
                (score, *n)
            })
            .filter(|(score, _)| *score >= MIN_SIMILARITY)
            .max_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((_, word)) = best {
            if !learned.iter().any(|w| w == word) {
                learned.push(word.to_string());
            }
        }
    }
    learned
}

/// A capital first letter means nothing when the word starts a sentence or a
/// line: "The" and "Now" there are grammar, not names.
fn looks_like_a_name(pasted: &str, fixed: &str, starts_sentence: bool) -> bool {
    let marked =
        fixed.chars().any(|c| c.is_ascii_digit()) || fixed.chars().skip(1).any(char::is_uppercase);
    if pasted.eq_ignore_ascii_case(fixed) || starts_sentence {
        marked
    } else {
        marked || fixed.chars().any(char::is_uppercase)
    }
}

/// Words that open a line or follow a full stop, question or exclamation mark.
fn sentence_starts(text: &str) -> Vec<&str> {
    let mut starts = Vec::new();
    for line in text.lines() {
        let mut at_start = true;
        for raw in line.split_whitespace().flat_map(split_sentences) {
            let word = raw.trim_matches(|c: char| !c.is_alphanumeric());
            if word.is_empty() {
                continue;
            }
            if at_start {
                starts.push(word);
            }
            at_start = raw
                .trim_end_matches(['"', '\'', ')', '\u{201D}', '\u{2019}'])
                .ends_with(['.', '!', '?']);
        }
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::corrected_words;

    #[test]
    fn learns_a_corrected_name() {
        assert_eq!(
            corrected_words(
                "Ask cubanetes about the deploy.",
                "Ask Kubernetes about the deploy."
            ),
            vec!["Kubernetes"]
        );
    }

    #[test]
    fn learns_inner_capitals_but_not_sentence_starts() {
        assert_eq!(
            corrected_words("open github now", "open GitHub now"),
            vec!["GitHub"]
        );
        assert!(corrected_words("the cat sat down", "The cat sat down").is_empty());
    }

    #[test]
    fn ignores_ordinary_word_fixes() {
        assert!(corrected_words("their going home now", "they're going home now").is_empty());
    }

    #[test]
    fn back_to_back_pastes_are_not_learned_as_names() {
        // Seen on a test laptop: the word list gained "lunch.Ask" and "update.Mr"
        // when the next dictation was pasted straight after the first one.
        assert!(corrected_words(
            "Push the page to staging before lunch.",
            "Push the page to staging before lunch.Ask Sidhartha to review it."
        )
        .is_empty());
        assert_eq!(
            corrected_words("build the aspnet app", "build the ASP.NET app"),
            vec!["ASP.NET"]
        );
    }

    #[test]
    fn sentence_starts_typed_after_a_paste_are_not_names() {
        // Seen on the owner's laptop: after a paste into a chat box and more
        // typing, the word list gained "The" (from "they") and "Now" (from "know").
        assert!(corrected_words(
            "not a bite more and they know it",
            "not a byte more and it is fine. The rest is new.\nNow I test it"
        )
        .is_empty());
        assert_eq!(
            corrected_words("ship it. kubernetes is up", "ship it. Kubernetes7 is up"),
            vec!["Kubernetes7"]
        );
    }

    #[test]
    fn learns_nothing_when_focus_moved_elsewhere() {
        assert!(corrected_words("send the report to Priya", "Inbox (3) Search mail").is_empty());
    }
}
