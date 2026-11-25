//! Turning a pasted or dropped file path into an attached picture.
//!
//! The keyboard route to attaching an image is not reliable and cannot be made
//! so: Windows Terminal binds `^v` to its own paste and resolves that binding
//! before a single byte reaches us — proved with the trace, which recorded the
//! keystroke typed straight after `^v` and no `^v`. codex has the same hole open
//! (issues #9999, #8607, #18645), and their answer is the one that works
//! everywhere, because it needs no key at all: you copy the *file*, or drag it
//! onto the window, and the terminal pastes its **path** as ordinary text. That
//! text does arrive.
//!
//! So this module reads a paste and decides whether it is really a picture.
//!
//! The decision here is deliberately syntactic — extensions and shapes, no
//! filesystem calls. A `is_file()` on a UNC path to a host that is down blocks
//! for tens of seconds, and this runs on the thread that draws.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine;

/// Extensions that make a paste a picture rather than a sentence. Deliberately
/// short: every one of these is a format the providers accept.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// Refuse past this, measured on the file. Roughly 6MB once base64 has added its
/// third, which is where providers start rejecting outright.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Paths in `pasted` that look like pictures, or nothing if it is just text.
///
/// All or nothing: a paste that is half paths and half prose is prose. Quietly
/// attaching one line of it and typing the rest would be the worst of both.
pub fn candidates(pasted: &str) -> Vec<PathBuf> {
    let whole = pasted.trim();
    if whole.is_empty() {
        return Vec::new();
    }
    // Several first, one last. `extension()` looks only past the final dot, so
    // "a.png\nb.jpg" read as a single path is a plausible-looking .jpg — trying
    // the whole string first would collapse every multi-file paste into one.
    //
    // One per line: Explorer's copy of several files, and every file manager
    // that writes a `text/uri-list`.
    let lines: Vec<&str> = whole.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() > 1 {
        if let Some(all) = all_images(&lines) {
            return all;
        }
    }
    // Space-separated, quoted where they need to be: dropping several files on
    // one window, and Explorer's "copy as path".
    let toks = tokens(whole);
    if toks.len() > 1 {
        let refs: Vec<&str> = toks.iter().map(String::as_str).collect();
        if let Some(all) = all_images(&refs) {
            return all;
        }
    }
    // One path, unquoted, spaces and all — what a bare drag gives on some
    // terminals, and the only reading left once splitting has been ruled out.
    if lines.len() == 1 {
        if let Some(p) = as_image_path(whole) {
            return vec![p];
        }
    }
    Vec::new()
}

fn all_images(parts: &[&str]) -> Option<Vec<PathBuf>> {
    let out: Vec<PathBuf> = parts.iter().filter_map(|p| as_image_path(p)).collect();
    (out.len() == parts.len()).then_some(out)
}

/// One token as a picture path, if that is what it is.
fn as_image_path(raw: &str) -> Option<PathBuf> {
    let s = unquote(raw.trim());
    if s.is_empty() {
        return None;
    }
    let s = if s.starts_with("file://") { from_file_url(&s)? } else { expand_home(&s) };
    let path = PathBuf::from(&s);
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !IMAGE_EXTS.contains(&ext.as_str()) {
        return None;
    }
    // A bare "photo.png" typed into a sentence is not a paste of a path. Require
    // something that says "location": a separator, or a drive letter.
    let looks_located = s.contains(['/', '\\']) || s.as_bytes().get(1) == Some(&b':');
    looks_located.then_some(path)
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    // Both kinds: cmd and Explorer use ", some shells use '.
    for q in ['"', '\''] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            return t[1..t.len() - 1].to_string();
        }
    }
    t.to_string()
}

/// `file:///C:/a%20b.png` → `C:/a b.png`, `file://host/share/x.png` → `\\host\share\x.png`.
fn from_file_url(s: &str) -> Option<String> {
    let rest = s.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    // An empty authority (`file:///…`) means a local path; anything else is a host.
    Some(match decoded.strip_prefix('/') {
        Some(local) => {
            // `/C:/x` is a Windows path wearing a URL; `/home/x` is not.
            if local.as_bytes().get(1) == Some(&b':') {
                local.to_string()
            } else {
                decoded.clone()
            }
        }
        None => format!("\\\\{}", decoded.replace('/', "\\")),
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    // A path that isn't UTF-8 after decoding is not one we can use anyway.
    String::from_utf8_lossy(&out).into_owned()
}

fn expand_home(s: &str) -> String {
    match s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        Some(rest) => match dirs::home_dir() {
            Some(h) => h.join(rest).display().to_string(),
            None => s.to_string(),
        },
        None => s.to_string(),
    }
}

/// Split on whitespace, honouring quotes: a dropped path with spaces in it
/// arrives quoted, and splitting it would leave two paths that exist nowhere.
fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match c {
            '"' | '\'' if quote.is_none() => quote = Some(c),
            c if Some(c) == quote => quote = None,
            c if c.is_whitespace() && quote.is_none() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Read a picture off disk as a `data:` URL. Slow enough to belong on a thread.
pub fn read_as_data_url(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("{} is not there", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a file", path.display());
    }
    if meta.len() > MAX_FILE_BYTES {
        bail!(
            "{} is {}MB — too large to send",
            path.display(),
            meta.len() / (1024 * 1024)
        );
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    // The extension got it this far; the bytes decide the type it is sent as. A
    // .png that is really a JPEG would be rejected by the provider with an error
    // about the picture rather than about its label.
    let mime = sniff(&bytes)
        .with_context(|| format!("{} is not an image the API takes", path.display()))?;
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

/// The format, from the bytes themselves.
fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if b.starts_with(b"BM") {
        return Some("image/bmp");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(s: &str) -> Option<String> {
        let c = candidates(s);
        (c.len() == 1).then(|| c[0].display().to_string())
    }

    #[test]
    fn a_plain_windows_path_is_a_picture() {
        assert_eq!(one(r"C:\Users\me\shot.png").as_deref(), Some(r"C:\Users\me\shot.png"));
    }

    #[test]
    fn a_quoted_path_with_spaces_survives_whole() {
        // What a drag onto the window gives, and what splitting on spaces ruins.
        assert_eq!(
            one("\"C:\\Users\\me\\due parole.png\"").as_deref(),
            Some(r"C:\Users\me\due parole.png")
        );
    }

    #[test]
    fn an_unquoted_path_with_spaces_survives_too() {
        assert_eq!(one(r"C:\a b\c d.jpg").as_deref(), Some(r"C:\a b\c d.jpg"));
    }

    #[test]
    fn a_file_url_becomes_a_path_and_loses_its_escapes() {
        assert_eq!(one("file:///C:/a%20b/shot.png").as_deref(), Some("C:/a b/shot.png"));
    }

    #[test]
    fn a_unix_file_url_keeps_its_leading_slash() {
        assert_eq!(one("file:///home/me/shot.png").as_deref(), Some("/home/me/shot.png"));
    }

    #[test]
    fn a_unc_file_url_becomes_a_unc_path() {
        assert_eq!(
            one("file://server/share/shot.png").as_deref(),
            Some(r"\\server\share\shot.png")
        );
    }

    #[test]
    fn several_paths_one_per_line_all_come_through() {
        let c = candidates("C:/a.png\nC:/b.jpg\n");
        assert_eq!(c.len(), 2);
        assert!(c[1].display().to_string().ends_with("b.jpg"));
    }

    #[test]
    fn several_quoted_paths_on_one_line_all_come_through() {
        let c = candidates("\"C:\\a b.png\" \"C:\\c.gif\"");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].display().to_string(), r"C:\a b.png");
    }

    #[test]
    fn prose_is_prose() {
        assert!(candidates("sistema il bug").is_empty());
        assert!(candidates("").is_empty());
        assert!(candidates("   \n  ").is_empty());
    }

    #[test]
    fn a_filename_inside_a_sentence_is_not_a_paste_of_a_path() {
        // Otherwise "guarda shot.png" would silently become an attachment.
        assert!(candidates("guarda shot.png e dimmi").is_empty());
        assert!(candidates("shot.png").is_empty(), "no separator, no location");
    }

    #[test]
    fn a_path_to_something_that_is_not_a_picture_stays_text() {
        assert!(candidates(r"C:\Users\me\note.txt").is_empty());
        assert!(candidates(r"C:\Users\me\archivio.zip").is_empty());
    }

    #[test]
    fn a_mix_of_paths_and_prose_is_all_prose() {
        // Attaching half a paste and typing the rest is the worst of both.
        assert!(candidates("C:/a.png e poi spiegami").is_empty());
        assert!(candidates("C:/a.png\nsistema il bug").is_empty());
    }

    #[test]
    fn the_extension_match_ignores_case() {
        assert!(one(r"C:\a\SHOT.PNG").is_some());
    }

    #[test]
    fn a_home_relative_path_is_expanded() {
        let Some(home) = dirs::home_dir() else { return };
        let got = one("~/shot.png").expect("not recognised");
        assert!(got.starts_with(&home.display().to_string()), "got {got}");
    }

    // ── reading ───────────────────────────────────────────────────────────────

    /// A one-pixel PNG, by hand — no encoder needed to prove the reader works.
    const PNG_1PX: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join("lrs-imgpath");
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    #[test]
    fn a_real_png_reads_back_as_a_data_url() {
        let p = scratch("ok.png");
        std::fs::write(&p, PNG_1PX).unwrap();
        let url = read_as_data_url(&p).unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "{}", &url[..40.min(url.len())]);
        assert!(url.len() > 40);
    }

    #[test]
    fn the_bytes_decide_the_type_not_the_extension() {
        // A provider rejecting a mislabelled picture reports something baffling
        // about the image; sending the right type avoids the whole conversation.
        let p = scratch("liar.png");
        std::fs::write(&p, [0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0]).unwrap();
        assert!(read_as_data_url(&p).unwrap().starts_with("data:image/jpeg;"));
    }

    #[test]
    fn something_that_is_not_an_image_is_refused_by_name() {
        let p = scratch("fake.png");
        std::fs::write(&p, b"non sono una figura").unwrap();
        let e = read_as_data_url(&p).unwrap_err().to_string();
        assert!(e.contains("not an image"), "got {e}");
    }

    #[test]
    fn a_missing_file_says_which_one() {
        let e = read_as_data_url(&scratch("mai-esistito.png")).unwrap_err().to_string();
        assert!(e.contains("mai-esistito.png"), "got {e}");
    }

    #[test]
    fn an_oversized_file_is_refused_before_it_is_read() {
        let p = scratch("enorme.png");
        std::fs::write(&p, vec![0u8; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        let e = read_as_data_url(&p).unwrap_err().to_string();
        assert!(e.contains("too large"), "got {e}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_directory_is_not_a_picture() {
        let d = scratch("cartella.png");
        std::fs::create_dir_all(&d).unwrap();
        assert!(read_as_data_url(&d).unwrap_err().to_string().contains("not a file"));
        let _ = std::fs::remove_dir(&d);
    }

    #[test]
    fn every_advertised_format_is_recognised() {
        assert_eq!(sniff(PNG_1PX), Some("image/png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"BM......"), Some("image/bmp"));
        assert_eq!(sniff(b"niente"), None);
        assert_eq!(sniff(b""), None);
    }
}
