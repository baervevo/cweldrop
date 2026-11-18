//! Loading files for the in-app viewer: raster images, SVG (rendered with
//! resvg) and PDF pages (rendered with poppler's `pdftoppm`) via the
//! terminal graphics protocol; text via `bat` (syntax highlighting);
//! anything else refused.
//!
//! Files usually come from peers, so everything here is defensive: sizes and
//! image dimensions are capped, nothing is executed, and the highlighted
//! text is stripped of control characters so a file can't smuggle escape
//! sequences to the terminal.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use ratatui::text::{Line, Span, Text};
use ratatui_image::picker::Picker;

use crate::event::ImageProto;
use crate::files;

/// Largest text file the viewer will load.
pub const MAX_TEXT_BYTES: u64 = 5 * 1024 * 1024;
/// Bytes sniffed to tell text from binary.
const SNIFF_BYTES: usize = 8 * 1024;

/// Largest SVG source the viewer will parse.
pub const MAX_SVG_BYTES: u64 = 10 * 1024 * 1024;
/// Longest side of a rendered SVG / PDF page, in pixels.
const RENDER_PX: f32 = 2048.0;
/// A PDF page that takes longer than this to render is given up on.
const PDF_TIMEOUT: Duration = Duration::from_secs(20);

/// What the viewer shows.
pub enum Content {
    /// A picture; `page` is `(current, total)` for paged documents.
    Image {
        proto: ImageProto,
        page: Option<(u32, u32)>,
    },
    Text(Text<'static>),
}

impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Image { page, .. } => write!(f, "Content::Image(page {page:?})"),
            Self::Text(t) => write!(f, "Content::Text({} lines)", t.lines.len()),
        }
    }
}

/// Load `path` (page `page`, 1-based, for PDFs) for display.
/// Blocking: call from `spawn_blocking`.
pub fn load(path: &Path, page: u32, picker: &Picker) -> Result<Content, String> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let picture = |img, page| Content::Image {
        proto: ImageProto(picker.new_resize_protocol(img)),
        page,
    };
    if files::is_image(name) {
        return Ok(picture(decode_image(path)?, None));
    }
    if files::is_svg(name) {
        return Ok(picture(render_svg(path)?, None));
    }
    if files::is_pdf(name) {
        let (img, total) = render_pdf_page(path, page)?;
        return Ok(picture(img, Some((page.clamp(1, total), total))));
    }
    load_text(path).map(Content::Text)
}

/// System fonts for SVG text, loaded once.
fn fontdb() -> Arc<resvg::usvg::fontdb::Database> {
    static DB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
    .clone()
}

/// Rasterise an SVG. External references (`<image href="/etc/...">`, other
/// files, URLs) are refused: a received SVG must not read local files.
pub fn render_svg(path: &Path) -> Result<image::DynamicImage, String> {
    use resvg::{tiny_skia, usvg};
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_SVG_BYTES {
        return Err(format!("SVG too large ({})", files::human_size(size)));
    }
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut opt = usvg::Options {
        resources_dir: None,
        fontdb: fontdb(),
        ..Default::default()
    };
    opt.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let tree = usvg::Tree::from_data(&data, &opt).map_err(|e| format!("bad SVG: {e}"))?;
    let (w, h) = (tree.size().width(), tree.size().height());
    // Fit the long side to RENDER_PX, scaling tiny icons up too.
    let scale = RENDER_PX / w.max(h).max(1.0);
    let (pw, ph) = ((w * scale).ceil().max(1.0), (h * scale).ceil().max(1.0));
    let mut pixmap = tiny_skia::Pixmap::new(pw as u32, ph as u32)
        .ok_or_else(|| "SVG has no drawable size".to_string())?;
    // White page behind transparent drawings, like a browser tab.
    pixmap.fill(tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let rgba = image::RgbaImage::from_raw(pixmap.width(), pixmap.height(), pixmap.take())
        .ok_or_else(|| "SVG render buffer mismatch".to_string())?;
    Ok(image::DynamicImage::ImageRgba8(rgba))
}

/// Number of pages per `pdfinfo`.
fn pdf_pages(path: &Path) -> Result<u32, String> {
    let out = run_with_timeout(Command::new("pdfinfo").arg("--").arg(path), PDF_TIMEOUT)
        .map_err(|e| format!("{e} (install poppler-utils to preview PDFs)"))?;
    String::from_utf8_lossy(&out)
        .lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|n| n.trim().parse().ok())
        .filter(|&n: &u32| n > 0)
        .ok_or_else(|| "not a readable PDF".to_string())
}

/// Render one page of a PDF with `pdftoppm`; returns it and the page count.
pub fn render_pdf_page(path: &Path, page: u32) -> Result<(image::DynamicImage, u32), String> {
    let total = pdf_pages(path)?;
    let page = page.clamp(1, total);
    let dir = std::env::temp_dir().join(format!("cweldrop-pdf-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let prefix = dir.join("page");
    let n = page.to_string();
    let res = run_with_timeout(
        Command::new("pdftoppm")
            .args(["-png", "-singlefile", "-scale-to"])
            .arg((RENDER_PX as u32).to_string())
            .args(["-f", &n, "-l", &n, "--"])
            .arg(path)
            .arg(&prefix),
        PDF_TIMEOUT,
    )
    .and_then(|_| decode_image(&prefix.with_extension("png")));
    let _ = std::fs::remove_dir_all(&dir);
    res.map(|img| (img, total))
}

/// Run a helper with no stdin and a deadline; stdout on success.
fn run_with_timeout(cmd: &mut Command, limit: Duration) -> Result<Vec<u8>, String> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run {name}: {e}"))?;
    // Drain stdout on a thread so a chatty child can't block on a full pipe.
    let mut stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(reader.join().unwrap_or_default()),
            Ok(Some(status)) => return Err(format!("{name} failed ({status})")),
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{name} timed out"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    }
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

    const SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10">
        <rect width="10" height="10" fill="#ff0000"/></svg>"##;

    #[test]
    fn svg_renders_scaled_with_colours() {
        let p = tmp_file("logo.svg", SVG);
        let img = render_svg(&p).unwrap().to_rgba8();
        assert_eq!(
            (img.width(), img.height()),
            (2048, 1024),
            "long side fitted"
        );
        assert_eq!(img.get_pixel(10, 10).0, [255, 0, 0, 255], "red rect");
        assert_eq!(
            img.get_pixel(2000, 10).0,
            [255, 255, 255, 255],
            "white page"
        );
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn svg_cannot_pull_in_local_files() {
        // A real PNG next to the SVG, referenced by relative and absolute path.
        let p = tmp_file("evil.svg", b"");
        let dir = p.parent().unwrap();
        let png = dir.join("secret.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 255, 255]))
            .save(&png)
            .unwrap();
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="8" height="4">
               <image href="secret.png" width="4" height="4"/>
               <image xlink:href="{}" x="4" width="4" height="4"/></svg>"#,
            png.display()
        );
        std::fs::write(&p, svg).unwrap();
        let img = render_svg(&p).unwrap().to_rgba8();
        assert!(
            img.pixels().all(|px| px.0 == [255, 255, 255, 255]),
            "referenced files must not be drawn"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn svg_garbage_is_an_error() {
        let p = tmp_file("bad.svg", b"not svg at all");
        assert!(render_svg(&p).unwrap_err().contains("bad SVG"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    /// Minimal two-page PDF (only exercised where poppler is installed).
    fn tiny_pdf() -> Vec<u8> {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] >>",
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
        for off in offsets {
            out.extend(format!("{off:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .bytes(),
        );
        out
    }

    #[test]
    fn pdf_pages_render_when_poppler_present() {
        if Command::new("pdftoppm").arg("-v").output().is_err() {
            eprintln!("skipping: pdftoppm not installed");
            return;
        }
        let p = tmp_file("doc.pdf", &tiny_pdf());
        let (p1, total) = render_pdf_page(&p, 1).unwrap();
        assert_eq!(total, 2);
        assert!(p1.width() > p1.height(), "page 1 is landscape");
        let (p2, _) = render_pdf_page(&p, 2).unwrap();
        assert!(p2.height() > p2.width(), "page 2 is portrait");
        let (_, _) = render_pdf_page(&p, 99).unwrap(); // clamped, no error
        let junk = tmp_file("junk.pdf", b"%PDF-1.4 nope");
        assert!(render_pdf_page(&junk, 1).is_err());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        let _ = std::fs::remove_dir_all(junk.parent().unwrap());
    }

    #[test]
    fn helper_timeout_kills_child() {
        let err = run_with_timeout(Command::new("sleep").arg("5"), Duration::from_millis(100))
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(run_with_timeout(
            &mut Command::new("definitely-not-a-binary"),
            Duration::from_secs(1)
        )
        .unwrap_err()
        .contains("cannot run"));
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
