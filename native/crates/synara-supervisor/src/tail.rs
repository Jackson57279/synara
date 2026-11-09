use std::collections::VecDeque;

/// Output kept to explain a failed start (`BACKEND_FAILURE_OUTPUT_TAIL_CHARS`).
pub const OUTPUT_TAIL_CHARS: usize = 8_192;
const SUMMARY_MAX_LINES: usize = 8;

/// Rolling window over the server's combined output, bounded so a chatty
/// crash loop cannot grow it.
#[derive(Debug, Default, Clone)]
pub struct OutputTail {
    chunks: VecDeque<String>,
    len: usize,
}

impl OutputTail {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &str) {
        let text = chunk.replace('\r', "");
        if text.is_empty() {
            return;
        }
        if text.len() >= OUTPUT_TAIL_CHARS {
            self.chunks.clear();
            self.chunks
                .push_back(suffix(&text, OUTPUT_TAIL_CHARS).to_owned());
            self.len = self.chunks[0].len();
            return;
        }
        self.len += text.len();
        self.chunks.push_back(text);
        while self.len - self.chunks.front().map_or(0, String::len) >= OUTPUT_TAIL_CHARS {
            if let Some(dropped) = self.chunks.pop_front() {
                self.len -= dropped.len();
            }
        }
    }

    pub fn read(&self) -> String {
        let joined: String = self.chunks.iter().map(String::as_str).collect();
        suffix(&joined, OUTPUT_TAIL_CHARS).to_owned()
    }

    pub fn clear(&mut self) {
        self.chunks.clear();
        self.len = 0;
    }
}

/// The last `max` bytes of `text`, moved forward to a char boundary.
fn suffix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// A few lines that explain a failure: starting at the last line mentioning
/// "error" when there is one, otherwise the final lines.
pub fn summarize_failure_output(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let start = lines
        .iter()
        .rposition(|line| line.to_ascii_lowercase().contains("error"))
        .unwrap_or_else(|| lines.len().saturating_sub(SUMMARY_MAX_LINES));
    lines[start..]
        .iter()
        .take(SUMMARY_MAX_LINES)
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_tail() {
        let mut tail = OutputTail::new();
        for i in 0..5_000 {
            tail.push(&format!("line {i}\r\n"));
        }
        let text = tail.read();
        assert!(text.len() <= OUTPUT_TAIL_CHARS);
        assert!(text.ends_with("line 4999\n"));
        assert!(!text.contains('\r'));
        tail.push(&"x".repeat(OUTPUT_TAIL_CHARS * 2));
        assert_eq!(tail.read().len(), OUTPUT_TAIL_CHARS);
    }

    #[test]
    fn suffix_respects_char_boundaries() {
        let mut tail = OutputTail::new();
        tail.push(&"é".repeat(OUTPUT_TAIL_CHARS));
        assert!(tail.read().chars().all(|c| c == 'é'));
    }

    #[test]
    fn summary_starts_at_the_last_error() {
        let output = "booting\nError: first\nmore\nError: EADDRINUSE 3773\n  at listen\n\n";
        assert_eq!(
            summarize_failure_output(output),
            "Error: EADDRINUSE 3773\n  at listen"
        );
        let quiet: String = (0..20).map(|i| format!("l{i}\n")).collect();
        assert_eq!(summarize_failure_output(&quiet).lines().count(), 8);
        assert!(summarize_failure_output(&quiet).starts_with("l12"));
        assert_eq!(summarize_failure_output(" \n"), "");
    }
}
