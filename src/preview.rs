//! Piped Kitty previews for file managers; the host owns terminal setup and cleanup.

// Rust guideline compliant 2026-10-07

use std::{
    borrow::Cow,
    ffi::OsString,
    io::{self, BufRead, Write},
    path::Path,
};

use anyhow::{Context, ensure};
use image::RgbImage;

use crate::{
    kitty::medium::{ChunkSize, Medium},
    renderer::PaneDocument,
};

/// Runs a preview from FILE WIDTH HEIGHT and reads page commands until EOF.
///
/// # Errors
/// Returns invalid argument, document rendering, or pipe errors.
pub(crate) fn run(args: Vec<OsString>) -> anyhow::Result<()> {
    let [path, width, height] = args.as_slice() else {
        anyhow::bail!("usage: kitpdf --preview FILE WIDTH HEIGHT");
    };
    let columns = dimension(width)?;
    let rows = dimension(height)?;
    let doc = PaneDocument::open(Path::new(path), columns, rows)?;
    serve(
        &doc,
        columns,
        rows,
        io::stdin().lock(),
        &mut io::stdout().lock(),
    )
}

fn dimension(value: &OsString) -> anyhow::Result<u16> {
    let value: u16 = value.to_str().context("pane size must be UTF-8")?.parse()?;
    // Cap raster memory even if a host accidentally passes pixel sizes as cells.
    ensure!(
        (1..=500).contains(&value),
        "pane size must be between 1 and 500 cells"
    );
    Ok(value)
}

fn serve(
    doc: &PaneDocument,
    columns: u16,
    rows: u16,
    mut input: impl BufRead,
    output: &mut impl Write,
) -> anyhow::Result<()> {
    let mut page = 0;
    write_frame(&doc.render(page)?, page as u32 + 1, columns, rows, output)?;
    loop {
        // Bound untrusted lines; valid commands are at most six bytes including LF.
        let mut command = String::new();
        let read = std::io::Read::take(&mut input, 64).read_line(&mut command)?;
        if read == 0 {
            return Ok(());
        }
        ensure!(
            command.ends_with('\n'),
            "unterminated or oversized preview command"
        );
        let next = page_for_command(page, doc.pages, command.trim());
        if next != page {
            let image = doc.render(next)?;
            write_frame(&image, next as u32 + 1, columns, rows, output)?;
            page = next;
        }
    }
}

fn page_for_command(page: usize, pages: usize, command: &str) -> usize {
    match command {
        "next" => page.saturating_add(1).min(pages.saturating_sub(1)),
        "prev" => page.saturating_sub(1),
        "first" => 0,
        "last" => pages.saturating_sub(1),
        _ => page,
    }
}

fn write_frame(
    image: &RgbImage,
    id: u32,
    columns: u16,
    rows: u16,
    output: &mut impl Write,
) -> io::Result<()> {
    // Keep the page aspect ratio using the same 10x20 cell model as rendering.
    let scale = (f64::from(columns) * 10.0 / f64::from(image.width()))
        .min(f64::from(rows) * 20.0 / f64::from(image.height()));
    let cols =
        ((f64::from(image.width()) * scale / 10.0).round() as u32).clamp(1, u32::from(columns));
    let lines =
        ((f64::from(image.height()) * scale / 20.0).round() as u32).clamp(1, u32::from(rows));
    let left = (u32::from(columns) - cols) / 2;
    if left > 0 {
        write!(output, "\x1b[{left}C")?;
    }
    // Different page ids let vfm show the new page before deleting the previous one.
    write!(
        output,
        "\x1b_Ga=T,i={id},f=24,s={},v={},c={cols},r={lines},C=1,q=2",
        image.width(),
        image.height()
    )?;
    Medium::Direct {
        data: Cow::Borrowed(image.as_raw()),
        chunk_size: Some(ChunkSize::default()),
    }
    .write_data(&mut *output)?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_commands_clamp_at_both_ends() {
        assert_eq!(page_for_command(0, 3, "prev"), 0);
        assert_eq!(page_for_command(2, 3, "next"), 2);
        assert_eq!(page_for_command(0, 3, "next"), 1);
        assert_eq!(page_for_command(2, 3, "prev"), 1);
        assert_eq!(page_for_command(1, 3, "first"), 0);
        assert_eq!(page_for_command(1, 3, "last"), 2);
        assert_eq!(page_for_command(1, 3, "unknown"), 1);
        assert_eq!(page_for_command(0, 1, "next"), 0);
    }

    #[test]
    fn pane_dimensions_are_bounded() {
        for value in ["0", "501", "-1", "65536", "bad"] {
            assert!(dimension(&value.into()).is_err());
        }
        assert_eq!(dimension(&"58".into()).unwrap(), 58);
    }

    #[test]
    fn frame_is_quiet_chunked_and_fits_the_pane() {
        let image = RgbImage::new(400, 600);
        let mut output = Vec::new();
        write_frame(&image, 1, 58, 30, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("\x1b[9C\x1b_Ga=T,i="));
        assert!(output.contains(",f=24,s=400,v=600,c=40,r=30,C=1,q=2,t=d,m=1;"));
        assert!(output.contains("\x1b_Gm=0;"));
        assert!(output.ends_with("\x1b\\"));
    }

    #[test]
    fn real_pdf_and_epub_turn_pages_and_exit_at_eof() {
        for name in ["preview-pages.pdf", "preview-pages.epub"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name);
            let doc = PaneDocument::open(&path, 58, 26).unwrap();
            assert!(doc.pages >= 2, "{name}");
            let mut output = Vec::new();
            serve(
                &doc,
                58,
                26,
                &b"prev\nnext\nprev\nlast\nnext\nfirst\nunknown\n"[..],
                &mut output,
            )
            .unwrap();
            let output = String::from_utf8(output).unwrap();
            let ids: Vec<u32> = output
                .split("\x1b_Ga=T,i=")
                .skip(1)
                .map(|frame| frame.split(',').next().unwrap().parse().unwrap())
                .collect();
            assert_eq!(ids, [1, 2, 1, doc.pages as u32, 1], "{name}");
            assert!(
                !output.contains("\x1b[?1049"),
                "must not own the alternate screen"
            );
            assert!(!output.contains("a=d"), "the host owns image cleanup");
        }
    }

    #[test]
    fn invalid_documents_and_oversized_commands_report_errors() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/preview-pages.pdf");
        let doc = PaneDocument::open(&path, 20, 10).unwrap();
        let mut output = Vec::new();
        assert!(serve(&doc, 20, 10, &vec![b'x'; 65][..], &mut output).is_err());
        assert!(PaneDocument::open(Path::new("missing-document.pdf"), 20, 10).is_err());
    }
}
