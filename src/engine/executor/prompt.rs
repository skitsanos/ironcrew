const PROMPT_TRUNCATION_MARKER: &str = "\n\n[... prompt truncated due to size limit]";

/// Incrementally builds a character-bounded prompt. This avoids first joining
/// every dependency/tool output into one unbounded temporary allocation and
/// only then truncating it.
pub(super) struct BoundedPrompt {
    text: String,
    max_chars: usize,
    chars: usize,
    truncated: bool,
}

impl BoundedPrompt {
    pub(super) fn new(max_chars: usize) -> Self {
        Self {
            text: String::with_capacity(max_chars.min(16 * 1024)),
            max_chars,
            chars: 0,
            truncated: false,
        }
    }

    fn push(&mut self, value: &str) {
        if self.truncated || self.chars >= self.max_chars {
            self.truncated |= !value.is_empty();
            return;
        }

        let remaining = self.max_chars - self.chars;
        let mut count = 0usize;
        let mut boundary = value.len();
        for (byte_index, _) in value.char_indices() {
            if count == remaining {
                boundary = byte_index;
                self.truncated = true;
                break;
            }
            count += 1;
        }
        self.text.push_str(&value[..boundary]);
        self.chars += count.min(remaining);
    }

    pub(super) fn section(&mut self, label: &str, value: &str) {
        if !self.text.is_empty() {
            self.push("\n\n");
        }
        self.push(label);
        self.push(value);
    }

    pub(super) fn finish(mut self) -> (String, bool) {
        if self.truncated {
            let marker_chars = PROMPT_TRUNCATION_MARKER.chars().count();
            let keep_chars = self.max_chars.saturating_sub(marker_chars);
            if let Some((boundary, _)) = self.text.char_indices().nth(keep_chars) {
                self.text.truncate(boundary);
            }
            let remaining = self.max_chars.saturating_sub(self.text.chars().count());
            let marker_boundary = PROMPT_TRUNCATION_MARKER
                .char_indices()
                .nth(remaining)
                .map(|(index, _)| index)
                .unwrap_or(PROMPT_TRUNCATION_MARKER.len());
            self.text
                .push_str(&PROMPT_TRUNCATION_MARKER[..marker_boundary]);
        }
        self.text.shrink_to_fit();
        (self.text, self.truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_prompt_truncation_is_character_safe_and_bounded() {
        let mut prompt = BoundedPrompt::new(12);
        prompt.section("Task: ", "🦀🦀🦀🦀🦀🦀🦀🦀");
        let (text, truncated) = prompt.finish();
        assert!(truncated);
        assert!(text.chars().count() <= 12);
        assert!(std::str::from_utf8(text.as_bytes()).is_ok());
    }

    #[test]
    fn prompt_builder_stops_copying_after_limit() {
        let mut prompt = BoundedPrompt::new(64);
        prompt.section("Task: ", &"x".repeat(1_000_000));
        prompt.section("Result: ", &"y".repeat(1_000_000));
        let (text, truncated) = prompt.finish();
        assert!(truncated);
        assert!(text.len() <= 64);
    }
}
