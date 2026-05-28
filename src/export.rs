use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use image::RgbImage;

use crate::error::RenderError;

const MAX_STEM_CHARS: usize = 48;

pub fn next_export_path(
    source_path: &Path,
    page_num: usize,
    n_pages: usize,
) -> std::io::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(next_export_path_in_dir(
        source_path,
        page_num,
        n_pages,
        &cwd,
    ))
}

pub fn next_export_path_in_dir(
    source_path: &Path,
    page_num: usize,
    n_pages: usize,
    dir: &Path,
) -> PathBuf {
    let page_number = page_num.saturating_add(1);
    let page_width = n_pages.max(1).to_string().len();
    let stem = sanitized_stem(source_path);
    let base = format!("{stem}-page-{page_number:0page_width$}");
    let mut candidate = dir.join(format!("{base}.png"));

    for suffix in 2.. {
        if !candidate.exists() {
            return candidate;
        }
        candidate = dir.join(format!("{base}-{suffix}.png"));
    }

    unreachable!()
}

pub fn write_png(path: &Path, img: &RgbImage) -> Result<(), RenderError> {
    img.save_with_format(path, image::ImageFormat::Png)
        .map_err(|e| RenderError::Converting(format!("cannot write PNG {}: {e}", path.display())))
}

fn sanitized_stem(source_path: &Path) -> String {
    let raw = source_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("document");

    let mut out = String::with_capacity(raw.len().min(MAX_STEM_CHARS));
    let mut last_dash = false;

    for ch in raw.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
            Some(ch.to_ascii_lowercase())
        } else if ch.is_whitespace() || ch == '-' {
            Some('-')
        } else {
            Some('-')
        };

        if let Some(ch) = mapped {
            if ch == '-' {
                if !last_dash {
                    out.push('-');
                    last_dash = true;
                }
            } else {
                out.push(ch);
                last_dash = false;
            }
        }
    }

    let mut cleaned = trim_export_edges(&out).to_owned();
    if cleaned.chars().count() > MAX_STEM_CHARS {
        let mut truncated = String::new();
        for ch in cleaned.chars().take(MAX_STEM_CHARS) {
            let _ = truncated.write_char(ch);
        }
        cleaned = trim_export_edges(&truncated).to_owned();
    }

    if cleaned.is_empty() {
        "document".to_owned()
    } else {
        cleaned
    }
}

fn trim_export_edges(s: &str) -> &str {
    s.trim_matches(|ch| matches!(ch, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("kitpdf-{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn export_filename_replaces_spaces_and_includes_page_number() {
        let dir = temp_dir("filename-spaces");
        let path = next_export_path_in_dir(Path::new("Long Paper Title.pdf"), 2, 120, &dir);

        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "long-paper-title-page-003.png"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn export_filename_shortens_long_pdf_names() {
        let dir = temp_dir("filename-long");
        let path = next_export_path_in_dir(
            Path::new("This is a very long PDF filename with many words and symbols!!!.pdf"),
            0,
            1,
            &dir,
        );
        let name = path.file_name().unwrap().to_str().unwrap();
        let stem = name.strip_suffix("-page-1.png").unwrap();

        assert!(stem.len() <= MAX_STEM_CHARS);
        assert!(!stem.contains(' '));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn export_filename_adds_collision_suffix() {
        let dir = temp_dir("filename-collision");
        let first = dir.join("paper-page-1.png");
        fs::write(&first, b"existing").unwrap();

        let path = next_export_path_in_dir(Path::new("paper.pdf"), 0, 1, &dir);

        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "paper-page-1-2.png"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_png_writes_png_file() {
        let dir = temp_dir("png-write");
        let path = dir.join("test.png");
        let img = RgbImage::from_pixel(2, 2, Rgb([10, 20, 30]));

        write_png(&path, &img).unwrap();
        let bytes = fs::read(&path).unwrap();

        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let _ = fs::remove_dir_all(dir);
    }
}
