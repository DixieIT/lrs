//! Reading an image out of the OS clipboard.
//!
//! A terminal never delivers an image through a paste — bracketed paste carries
//! text and nothing else — so the only way to get a screenshot into a turn is to
//! ask the operating system directly.
//!
//! The clipboard hands over raw RGBA; providers want a `data:` URL, so it is
//! re-encoded as PNG here. Both steps are slow enough to matter at 30fps, which
//! is why the caller runs this on a thread.

use anyhow::{bail, Context, Result};
use base64::Engine;

/// Refuse anything past this once encoded. A 4K screenshot is ~10MB of base64,
/// which most providers reject outright and every provider bills for.
const MAX_BYTES: usize = 6 * 1024 * 1024;

/// Terminals cap the OSC-52 sequence, and the limit varies. Past this the escape
/// route is skipped and the native clipboard carries it alone.
const OSC52_LIMIT: usize = 64 * 1024;

/// Put `text` on the clipboard, both ways, because neither alone is enough:
/// OSC 52 hands it to the *terminal*, which is the only thing that works over ssh
/// and doesn't depend on this process staying alive; the native clipboard is the
/// only thing that works in terminals with OSC 52 disabled (Windows Terminal ships
/// it off by default).
///
/// Returns the number of characters copied. Only a total failure is an error —
/// one route working is a successful copy.
pub fn copy(text: &str) -> Result<usize> {
    if text.is_empty() {
        bail!("nothing selected");
    }
    let osc = if text.len() <= OSC52_LIMIT { osc52(text).is_ok() } else { false };
    let native = set_native(text).is_ok();
    if !osc && !native {
        bail!("no clipboard available");
    }
    Ok(text.chars().count())
}

/// The clipboard handle, kept for the life of the process.
///
/// On X11 and some Wayland compositors the *owning process* serves the clipboard
/// on demand, so dropping the handle before the user pastes makes what was copied
/// vanish. (This is codex's `ClipboardLease` — their comment is the clearest
/// statement of it, and the bug is invisible on Windows and macOS, where the
/// clipboard takes a copy of the data.) Reusing one handle also spares every copy
/// a fresh connection to the display server.
///
/// Poisoning is ignored: a panic in another thread's copy is no reason to stop
/// this one working.
fn set_native(text: &str) -> Result<()> {
    use std::sync::{Mutex, OnceLock};
    static HELD: OnceLock<Mutex<Option<arboard::Clipboard>>> = OnceLock::new();
    let cell = HELD.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(arboard::Clipboard::new().context("opening the clipboard")?);
    }
    let cb = guard.as_mut().expect("just filled");
    match cb.set_text(text.to_string()) {
        Ok(()) => Ok(()),
        Err(e) => {
            // A handle that has gone bad stays bad; drop it so the next copy gets
            // a fresh one rather than failing forever.
            *guard = None;
            Err(anyhow::Error::new(e).context("writing to the clipboard"))
        }
    }
}

/// `ESC ] 52 ; c ; <base64> BEL` — the terminal's own copy request.
fn osc52(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let payload = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout().lock();
    write!(out, "\x1b]52;c;{payload}\x07")?;
    out.flush()
}

/// The clipboard's image as a `data:image/png;base64,…` URL.
pub fn image_data_url() -> Result<String> {
    let mut cb = arboard::Clipboard::new().context("no clipboard")?;
    let img = cb.get_image().context("the clipboard holds no image")?;
    let png = encode_png(img.width, img.height, &img.bytes)?;
    if png.len() > MAX_BYTES {
        bail!(
            "image is {}MB encoded — too large to send",
            png.len() / (1024 * 1024)
        );
    }
    Ok(to_data_url(&png))
}

/// RGBA8 → PNG bytes.
fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>> {
    if width == 0 || height == 0 {
        bail!("clipboard image has no pixels");
    }
    // A short buffer would make the encoder write uninitialised rows.
    let want = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .context("clipboard image dimensions overflow")?;
    if rgba.len() < want {
        bail!("clipboard image is truncated ({} of {want} bytes)", rgba.len());
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width as u32, height as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().context("writing the PNG header")?;
        writer.write_image_data(&rgba[..want]).context("writing PNG pixels")?;
    }
    Ok(out)
}

fn to_data_url(png: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_solid_square_encodes_to_a_real_png() {
        let rgba = vec![255u8; 4 * 4 * 4]; // 4×4 white
        let png = encode_png(4, 4, &rgba).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG signature");
        assert!(png.len() > 8);
    }

    #[test]
    fn a_truncated_buffer_is_refused_not_encoded() {
        // Encoding past the end of the buffer would emit garbage rows.
        let err = encode_png(4, 4, &[0u8; 10]).unwrap_err().to_string();
        assert!(err.contains("truncated"), "got {err:?}");
    }

    #[test]
    fn zero_sized_images_are_refused() {
        assert!(encode_png(0, 10, &[]).is_err());
        assert!(encode_png(10, 0, &[]).is_err());
    }

    #[test]
    fn extra_trailing_bytes_do_not_break_the_encode() {
        // Some clipboards pad the buffer; only width*height*4 is ours to read.
        let rgba = vec![1u8; 4 * 2 * 2 + 64];
        assert!(encode_png(2, 2, &rgba).is_ok());
    }

    #[test]
    fn the_data_url_is_shaped_the_way_providers_expect() {
        let url = to_data_url(&[0u8, 1, 2]);
        assert!(url.starts_with("data:image/png;base64,"), "got {url:?}");
        let payload = url.trim_start_matches("data:image/png;base64,");
        let back = base64::engine::general_purpose::STANDARD.decode(payload).unwrap();
        assert_eq!(back, vec![0u8, 1, 2]);
    }
}
