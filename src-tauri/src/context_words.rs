//! Context words: names and jargon read from the screen when recording starts,
//! added to the custom words for that one dictation only.
//!
//! On Windows, UI Automation reads the focused text box and the element under
//! the mouse pointer ("point and talk"). The read runs on its own thread so a
//! slow or hung app never delays recording; if it has not finished when the
//! transcription runs, that dictation simply goes without context words.

use crate::managers::transcription::TranscriptionManager;
use std::sync::Arc;

/// At most this many context words join the custom words per dictation.
const MAX_CONTEXT_WORDS: usize = 20;

/// Characters read from each on-screen element.
#[cfg(windows)]
const MAX_ELEMENT_CHARS: i32 = 4000;

/// Starts reading on-screen context in the background and hands the
/// distinctive words to the transcription manager.
pub fn spawn_capture(tm: Arc<TranscriptionManager>) {
    let capture = tm.begin_context_capture();
    std::thread::spawn(move || {
        let text = read_screen_text();
        let words = distinctive_words(&text, MAX_CONTEXT_WORDS);
        log::debug!("Context words from screen: {}", words.len());
        tm.set_context_words(capture, words);
    });
}

#[cfg(windows)]
fn read_screen_text() -> String {
    match windows_uia::read_focus_and_pointer() {
        Ok(text) => text,
        Err(e) => {
            log::debug!("Context read failed: {}", e);
            String::new()
        }
    }
}

#[cfg(not(windows))]
fn read_screen_text() -> String {
    String::new()
}

#[cfg(windows)]
mod windows_uia {
    use super::MAX_ELEMENT_CHARS;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
        IUIAutomationValuePattern, UIA_TextPatternId, UIA_ValuePatternId,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    /// Text of the focused element plus the element under the mouse pointer.
    pub fn read_focus_and_pointer() -> windows::core::Result<String> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let result = read_with_com();
            CoUninitialize();
            result
        }
    }

    unsafe fn read_with_com() -> windows::core::Result<String> {
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let mut text = String::new();
        if let Ok(focused) = uia.GetFocusedElement() {
            text.push_str(&element_text(&focused));
        }
        let mut pointer = POINT::default();
        if GetCursorPos(&mut pointer).is_ok() {
            if let Ok(under_mouse) = uia.ElementFromPoint(pointer) {
                text.push('\n');
                text.push_str(&element_text(&under_mouse));
            }
        }
        Ok(text)
    }

    /// Document text, else the value, else the name. Password boxes are never read.
    unsafe fn element_text(element: &IUIAutomationElement) -> String {
        if element
            .CurrentIsPassword()
            .map(|b| b.as_bool())
            .unwrap_or(true)
        {
            return String::new();
        }
        if let Ok(pattern) =
            element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
        {
            if let Ok(text) = pattern
                .DocumentRange()
                .and_then(|r| r.GetText(MAX_ELEMENT_CHARS))
            {
                return text.to_string();
            }
        }
        if let Ok(pattern) =
            element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
        {
            if let Ok(value) = pattern.CurrentValue() {
                return value.to_string();
            }
        }
        element
            .CurrentName()
            .map(|n| n.to_string())
            .unwrap_or_default()
    }
}

/// Words likely to be names or jargon: a capital letter mid-sentence, a
/// capital after the first letter (CamelCase, acronyms), or letters mixed with
/// digits. Ordinary words would only make the fuzzy correction over-eager.
pub fn distinctive_words(text: &str, limit: usize) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut sentence_start = true;
    for raw in text.split_whitespace() {
        let token = raw.trim_matches(|c: char| !c.is_alphanumeric());
        let starts_sentence = sentence_start;
        sentence_start = raw.ends_with(['.', '!', '?', ':']);
        if token.chars().count() < 3 || !token.chars().any(char::is_alphabetic) {
            continue;
        }
        let mut chars = token.chars();
        let first_upper = chars.next().is_some_and(char::is_uppercase);
        let inner_upper = chars.any(char::is_uppercase);
        let has_digit = token.chars().any(|c| c.is_ascii_digit());
        let distinctive = inner_upper || has_digit || (first_upper && !starts_sentence);
        if distinctive && !words.iter().any(|w| w.eq_ignore_ascii_case(token)) {
            words.push(token.to_string());
            if words.len() == limit {
                break;
            }
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::distinctive_words;

    #[test]
    fn keeps_names_and_jargon_mid_sentence() {
        let text = "Hi team, please ask Priya about the Simplotel invoice INV4471 before Friday.";
        assert_eq!(
            distinctive_words(text, 20),
            vec!["Priya", "Simplotel", "INV4471", "Friday"]
        );
    }

    #[test]
    fn skips_sentence_starts_short_words_and_duplicates() {
        let text = "The deck is ready. Send it to Rahul. Rahul said OK. We use TypeScript.";
        assert_eq!(distinctive_words(text, 20), vec!["Rahul", "TypeScript"]);
    }

    #[test]
    fn stops_at_the_limit() {
        let text = "x Alpha Bravo Charlie Delta";
        assert_eq!(distinctive_words(text, 2), vec!["Alpha", "Bravo"]);
    }
}
