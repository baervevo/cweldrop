//! URL detection in chat messages and opening them in the system browser.
//!
//! Only `http://`, `https://` and bare `www.` links are recognised, and only
//! those are ever handed to the OS opener, so a peer can't make us launch
//! `file://` paths or custom URL handlers. The URL is passed as a single argv
//! entry (no shell), and the opener's stdio is nulled so it can't scribble
//! over the TUI.

use std::ops::Range;
use std::process::{Command, Stdio};

const TRAILING_PUNCT: &[char] = &['.', ',', ';', ':', '!', '?', '\'', '"'];

/// Byte ranges of every URL in `text`, in order of appearance.
pub fn find_urls(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut offset = 0;
    for token in text.split_inclusive(char::is_whitespace) {
        let tok_start = offset;
        offset += token.len();
        let token = token.trim_end();
        let start = ["https://", "http://", "www."]
            .iter()
            .filter_map(|p| token.find(p))
            .min();
        let Some(start) = start else { continue };
        let url = trim_url(&token[start..]);
        if normalize(url).is_some() {
            out.push(tok_start + start..tok_start + start + url.len());
        }
    }
    out
}

/// Drop trailing sentence punctuation and closing brackets that have no
/// matching opener inside the URL (e.g. "(see https://x.org/a)").
fn trim_url(mut url: &str) -> &str {
    loop {
        let Some(last) = url.chars().last() else {
            return url;
        };
        let unbalanced = |open: char, close: char| {
            last == close && url.matches(close).count() > url.matches(open).count()
        };
        if TRAILING_PUNCT.contains(&last)
            || unbalanced('(', ')')
            || unbalanced('[', ']')
            || unbalanced('{', '}')
            || last == '>'
        {
            url = &url[..url.len() - last.len_utf8()];
        } else {
            return url;
        }
    }
}

/// Canonical openable form of `url`, or `None` if it isn't a web link we're
/// willing to open.
pub fn normalize(url: &str) -> Option<String> {
    let full = if url.starts_with("www.") {
        format!("https://{url}")
    } else {
        url.to_string()
    };
    let rest = full
        .strip_prefix("https://")
        .or_else(|| full.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.starts_with('.') || full.chars().any(char::is_control) {
        return None;
    }
    Some(full)
}

/// Every URL in `text`, normalised, in order.
pub fn urls_in(text: &str) -> Vec<String> {
    find_urls(text)
        .into_iter()
        .filter_map(|r| normalize(&text[r]))
        .collect()
}

/// Open `url` in the default browser. Fire-and-forget; failures are logged.
pub fn open(url: &str) {
    let Some(url) = normalize(url) else {
        tracing::warn!(%url, "refusing to open non-web link");
        return;
    };
    std::thread::spawn(move || {
        let mut cmd = opener();
        cmd.arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match cmd.status() {
            Ok(s) if s.success() => tracing::info!(%url, "opened link"),
            Ok(s) => tracing::warn!(%url, status = %s, "link opener failed"),
            Err(e) => tracing::warn!(%url, error = %e, "could not run link opener"),
        }
    });
}

fn opener() -> Command {
    if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        Command::new("xdg-open")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Vec<&str> {
        find_urls(text).into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn finds_plain_urls() {
        assert_eq!(
            found("see https://example.com/a?b=1 and http://x.org"),
            vec!["https://example.com/a?b=1", "http://x.org"]
        );
        assert_eq!(found("www.rust-lang.org rocks"), vec!["www.rust-lang.org"]);
    }

    #[test]
    fn trims_punctuation_and_unbalanced_brackets() {
        assert_eq!(found("go to https://a.com."), vec!["https://a.com"]);
        assert_eq!(found("(https://a.com/x)"), vec!["https://a.com/x"]);
        assert_eq!(
            found("https://en.wikipedia.org/wiki/Rust_(language)!"),
            vec!["https://en.wikipedia.org/wiki/Rust_(language)"]
        );
        assert_eq!(found("<https://a.com>"), vec!["https://a.com"]);
    }

    #[test]
    fn byte_offsets_survive_multibyte_text() {
        let text = "zażółć https://ł.pl/ż gęślą";
        assert_eq!(found(text), vec!["https://ł.pl/ż"]);
    }

    #[test]
    fn ignores_non_links() {
        assert!(found("no links here").is_empty());
        assert!(found("https:// alone").is_empty());
        assert!(found("file:///etc/passwd").is_empty());
        assert!(found("javascript:alert(1)").is_empty());
    }

    #[test]
    fn normalize_only_accepts_web_links() {
        assert_eq!(normalize("www.a.com").as_deref(), Some("https://www.a.com"));
        assert_eq!(normalize("http://a.com").as_deref(), Some("http://a.com"));
        assert_eq!(normalize("file:///etc/passwd"), None);
        assert_eq!(normalize("https://"), None);
        assert_eq!(normalize("https://a.com\u{1b}[2J"), None);
    }
}
