/// True-negative test fixture: clean Rust utility code.
///
/// Expected audit verdict: **pass**
/// No security issues — standard string manipulation helpers.

/// Truncates a string to at most `max_chars` characters, appending an ellipsis
/// if truncation occurred.
pub fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        let end = s
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}

/// Returns the first `n` lines of `text`, separated by newlines.
pub fn first_n_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let result = truncate("hello world", 5);
        assert!(result.starts_with("hello"));
        assert!(result.contains('…'));
    }

    #[test]
    fn first_lines() {
        let text = "line1\nline2\nline3\nline4";
        assert_eq!(first_n_lines(text, 2), "line1\nline2");
    }
}
