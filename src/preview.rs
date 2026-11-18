//! Loading files for the in-app viewer: images via the terminal graphics
//! protocol, text via `bat` (syntax highlighting), anything else refused.
//!
//! Files usually come from peers, so everything here is defensive: sizes and
//! image dimensions are capped, nothing is executed, and the highlighted
//! text is stripped of control characters so a file can't smuggle escape
//! sequences to the terminal.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use ansi_to_tui::IntoText;
use ratatui::text::{Line, Span, Text};
use ratatui_image::picker::Picker;

use crate::event::ImageProto;
use crate::files;

/// Largest text file the viewer will load.
pub const MAX_TEXT_BYTES: u64 = 5 * 1024 * 1024;
/// Bytes sniffed to tell text from binary.
const SNIFF_BYTES: usize = 8 * 1024;

/// What the viewer shows.
pub enum Content {
    Image(ImageProto),
    Text(Text<'static>),
}

impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Image(_) => f.write_str("Content::Image"),
            Self::Text(t) => write!(f, "Content::Text({} lines)", t.lines.len()),
        }
    }
}

/// Load `path` for display. Blocking: call from `spawn_blocking`.
pub fn load(path: &Path, picker: &Picker) -> Result<Content, String> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if files::is_image(name) {
        let img = decode_image(path)?;
        return Ok(Content::Image(ImageProto(picker.new_resize_protocol(img))));
    }
    load_text(path).map(Content::Text)
}

/// Decode an image, capping dimensions and allocation so a crafted file
/// can't exhaust memory.
pub fn decode_image(path: &Path) -> Result<image::DynamicImage, String> {
    let mut reader = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    // Nothing on a terminal needs more than this; keeps re-encoding cheap.
    Ok(if img.width() > 4096 || img.height() > 4096 {
        img.thumbnail(4096, 4096)
    } else {
        img
    })
}

/// Highlighted text of `path`, or an error for binary / oversized files.
pub fn load_text(path: &Path) -> Result<Text<'static>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "too large to preview ({}); saved at {}",
            files::human_size(meta.len()),
            path.display()
        ));
    }
    if !looks_like_text(path)? {
        return Err(format!("binary file; saved at {}", path.display()));
    }
    let text = match run_bat(path) {
        Some(ansi) => ansi.into_text().map_err(|e| e.to_string())?,
        None => {
            let raw = std::fs::read(path).map_err(|e| e.to_string())?;
            Text::raw(String::from_utf8_lossy(&raw).into_owned())
        }
    };
    Ok(sanitize(text))
}

fn looks_like_text(path: &Path) -> Result<bool, String> {
    let mut buf = vec![0u8; SNIFF_BYTES];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut buf))
        .map_err(|e| e.to_string())?;
    let head = &buf[..n];
    if head.contains(&0) {
        return Ok(false);
    }
    // Allow a multi-byte char cut off at the sniff boundary.
    Ok(match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    })
}

/// `bat` output with ANSI colours, or `None` if bat isn't installed or fails.
fn run_bat(path: &Path) -> Option<Vec<u8>> {
    for bin in ["bat", "batcat"] {
        let out = Command::new(bin)
            .args([
                "--color=always",
                "--style=numbers",
                "--paging=never",
                "--wrap=never",
                "--",
            ])
            .arg(path)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => return Some(o.stdout),
            Ok(o) => {
                tracing::debug!(%bin, status = %o.status, "bat failed");
                return None;
            }
            Err(_) => continue, // not installed under this name
        }
    }
    None
}

/// Expand tabs and drop control characters from rendered text. Styles from
/// the ANSI parse are kept; raw bytes never reach the terminal.
fn sanitize(text: Text<'static>) -> Text<'static> {
    let lines = text
        .lines
        .into_iter()
        .map(|line| {
            let spans = line
                .spans
                .into_iter()
                .map(|s| {
                    let clean: String = s
                        .content
                        .chars()
                        .flat_map(|c| match c {
                            '\t' => "    ".chars().collect::<Vec<_>>(),
                            c if c.is_control() => Vec::new(),
                            c => vec![c],
                        })
                        .collect();
                    Span::styled(clean, s.style)
                })
                .collect::<Vec<_>>();
            Line::from(spans).style(line.style)
        })
        .collect::<Vec<_>>();
    Text::from(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cweldrop-preview-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn plain(t: &Text) -> String {
        t.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn text_file_renders_with_content() {
        let p = tmp_file("main.rs", b"fn main() {\n\tprintln!(\"hi\");\n}\n");
        let t = load_text(&p).unwrap();
        let s = plain(&t);
        assert!(s.contains("fn main()"), "{s}");
        assert!(s.contains("    println!"), "tabs expanded: {s}");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn escape_sequences_in_files_are_neutralised() {
        let p = tmp_file("evil.txt", b"before\x1b]0;pwned\x07\x1b[2Jafter\r\n");
        let t = load_text(&p).unwrap();
        for line in &t.lines {
            for span in &line.spans {
                assert!(
                    !span.content.chars().any(|c| c.is_control()),
                    "control char survived: {:?}",
                    span.content
                );
            }
        }
        assert!(plain(&t).contains("before"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn binary_and_large_files_refused() {
        let p = tmp_file("blob.bin", &[0x7f, b'E', b'L', b'F', 0, 1, 2]);
        assert!(load_text(&p).unwrap_err().contains("binary"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        let big = tmp_file("big.txt", &vec![b'a'; MAX_TEXT_BYTES as usize + 1]);
        assert!(load_text(&big).unwrap_err().contains("too large"));
        let _ = std::fs::remove_dir_all(big.parent().unwrap());
    }

    #[test]
    fn sanitize_keeps_styles() {
        use ratatui::style::{Color, Style};
        let t = Text::from(Line::from(Span::styled(
            "a\x07b",
            Style::default().fg(Color::Red),
        )));
        let out = sanitize(t);
        assert_eq!(out.lines[0].spans[0].content, "ab");
        assert_eq!(out.lines[0].spans[0].style.fg, Some(Color::Red));
    }
}
