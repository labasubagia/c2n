//! PNG prompt metadata injection.

use std::path::Path;

use crate::adapters::filesystem::SafeFileTarget;
use img_parts::Bytes;
use img_parts::png::{Png, PngChunk};

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("PNG error: {0}")]
    Png(#[from] img_parts::Error),
    #[error("PNG has no IEND chunk")]
    MissingIend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageOutcome {
    Written,
    AlreadyPresent,
    SkippedNotPng,
}

/// Read the embedded `Prompt` text from a PNG, if present.
pub fn get_png_prompt(path: &Path) -> Result<Option<String>, ImageError> {
    let bytes = std::fs::read(path)?;
    let png = Png::from_bytes(bytes.into())?;
    for chunk in png.chunks() {
        if chunk_keyword(chunk).as_deref() == Some("Prompt") {
            return Ok(chunk_text(chunk));
        }
    }
    Ok(None)
}

/// Inject `prompt` into a PNG's `tEXt` or `iTXt` metadata idempotently.
///
/// The write is atomic: a temporary file in the same directory is persisted
/// over the original only after the complete PNG has been encoded.
pub fn add_prompt_to_image(path: &Path, prompt: &str) -> Result<ImageOutcome, ImageError> {
    let target = SafeFileTarget::open(path)
        .map_err(|error| ImageError::Io(std::io::Error::other(error.to_string())))?;
    add_prompt_to_target(&target, path, prompt)
}

pub(crate) fn add_prompt_to_target(
    target: &SafeFileTarget,
    path: &Path,
    prompt: &str,
) -> Result<ImageOutcome, ImageError> {
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
    {
        return Ok(ImageOutcome::SkippedNotPng);
    }

    let mut bytes = Vec::new();
    let mut source = target
        .open_read()
        .map_err(|error| ImageError::Io(std::io::Error::other(error.to_string())))?;
    std::io::Read::read_to_end(&mut source, &mut bytes)?;
    let mut png = Png::from_bytes(bytes.into())?;

    let existing: Vec<Option<String>> = png
        .chunks()
        .iter()
        .filter(|chunk| chunk_keyword(chunk).as_deref() == Some("Prompt"))
        .map(chunk_text)
        .collect();

    if existing.len() == 1 && existing[0].as_deref() == Some(prompt) {
        return Ok(ImageOutcome::AlreadyPresent);
    }

    png.chunks_mut()
        .retain(|chunk| chunk_keyword(chunk).as_deref() != Some("Prompt"));

    let chunk =
        if prompt.chars().all(|character| (character as u32) <= 0xFF) && !prompt.contains('\0') {
            let mut contents = Vec::with_capacity(7 + prompt.len());
            contents.extend_from_slice(b"Prompt\0");
            contents.extend(prompt.chars().map(|character| character as u8));
            PngChunk::new(*b"tEXt", Bytes::from(contents))
        } else {
            let mut contents = Vec::with_capacity(11 + prompt.len());
            contents.extend_from_slice(b"Prompt\0\x00\x00\x00\x00");
            contents.extend_from_slice(prompt.as_bytes());
            PngChunk::new(*b"iTXt", Bytes::from(contents))
        };

    let iend_position = png
        .chunks()
        .iter()
        .position(|existing| existing.kind() == *b"IEND")
        .ok_or(ImageError::MissingIend)?;
    png.chunks_mut().insert(iend_position, chunk);

    let mut temporary = target
        .create_temp()
        .map_err(|error| ImageError::Io(std::io::Error::other(error.to_string())))?;
    png.encoder().write_to(temporary.file_mut())?;
    target
        .replace(temporary)
        .map_err(|error| ImageError::Io(std::io::Error::other(error.to_string())))?;

    Ok(ImageOutcome::Written)
}

fn chunk_keyword(chunk: &PngChunk) -> Option<String> {
    let kind = chunk.kind();
    if kind != *b"tEXt" && kind != *b"iTXt" {
        return None;
    }
    let contents = chunk.contents();
    let end = contents.iter().position(|byte| *byte == 0)?;
    Some(String::from_utf8_lossy(&contents[..end]).into_owned())
}

fn chunk_text(chunk: &PngChunk) -> Option<String> {
    let kind = chunk.kind();
    let contents = chunk.contents();

    if kind == *b"tEXt" {
        let keyword_end = contents.iter().position(|byte| *byte == 0)?;
        let text = contents.get(keyword_end + 1..)?;
        return Some(text.iter().map(|byte| *byte as char).collect());
    }

    if kind == *b"iTXt" {
        let keyword_end = contents.iter().position(|byte| *byte == 0)?;
        let rest = contents.get(keyword_end + 1..)?;
        if rest.len() < 4 {
            return None;
        }
        let compression_flag = rest[0];
        let compression_method = rest[1];
        if compression_flag != 0 || compression_method != 0 {
            return None;
        }
        let rest = &rest[2..];
        let language_end = rest.iter().position(|byte| *byte == 0)?;
        let rest = &rest[language_end + 1..];
        let translated_end = rest.iter().position(|byte| *byte == 0)?;
        let text = &rest[translated_end + 1..];
        return Some(String::from_utf8_lossy(text).into_owned());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png() -> Vec<u8> {
        // 1x1 transparent PNG.
        const BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(BASE64)
            .unwrap()
    }

    #[test]
    fn injects_and_reads_latin1_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("image.png");
        std::fs::write(&path, sample_png()).unwrap();

        assert_eq!(
            add_prompt_to_image(&path, "a cat").unwrap(),
            ImageOutcome::Written
        );
        assert_eq!(get_png_prompt(&path).unwrap().as_deref(), Some("a cat"));
        assert_eq!(
            add_prompt_to_image(&path, "a cat").unwrap(),
            ImageOutcome::AlreadyPresent
        );
    }

    #[test]
    fn injects_utf8_prompt_via_itxt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("image.png");
        std::fs::write(&path, sample_png()).unwrap();

        let prompt = "a cat with a 🐟 and café";
        assert_eq!(
            add_prompt_to_image(&path, prompt).unwrap(),
            ImageOutcome::Written
        );
        assert_eq!(get_png_prompt(&path).unwrap().as_deref(), Some(prompt));
    }

    #[test]
    fn skips_non_png_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("image.webp");
        std::fs::write(&path, b"not an image").unwrap();
        assert_eq!(
            add_prompt_to_image(&path, "prompt").unwrap(),
            ImageOutcome::SkippedNotPng
        );
    }
}
