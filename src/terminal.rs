// Terminal setup/teardown and utility drawing functions.
// Manages raw mode, alternate screen, and window size detection without ratatui.

use std::{
    io::{self, Write as _, stdout},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use crossterm::{
    cursor, execute,
    style::Print,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, is_raw_mode_enabled, window_size,
    },
};

/// Terminal dimensions in both cells and pixels.
#[derive(Debug, Clone, Copy)]
pub struct WindowSize {
    pub rows: u16,
    pub cols: u16,
    /// Total terminal width in pixels.
    pub width_px: u16,
    /// Width of a single cell in pixels.
    pub cell_width_px: u16,
    /// Height of a single cell in pixels.
    pub cell_height_px: u16,
}

impl WindowSize {
    /// Pixel width of the page display area (full terminal width).
    pub fn page_area_width_px(&self) -> f32 {
        self.width_px as f32
    }

    /// Pixel height of the page display area (terminal minus one row for status bar).
    pub fn page_area_height_px(&self) -> f32 {
        ((self.rows.saturating_sub(1)) as f32) * (self.cell_height_px as f32)
    }

    /// Number of rows available for page display (rows minus status bar).
    pub fn page_rows(&self) -> u16 {
        self.rows.saturating_sub(1)
    }
}

/// RAII guard that enters the alternate screen and raw mode on construction,
/// and restores the terminal on drop.
pub struct TerminalGuard;

#[cfg(windows)]
pub(crate) struct ConsoleInputMode {
    mode: crossterm_winapi::ConsoleMode,
    original: u32,
}

#[cfg(windows)]
impl ConsoleInputMode {
    fn capture() -> io::Result<Self> {
        let mode =
            crossterm_winapi::ConsoleMode::from(crossterm_winapi::Handle::current_in_handle()?);
        let original = mode.mode()?;
        Ok(Self { mode, original })
    }

    fn enable_vt(&self) -> io::Result<()> {
        // Preserve terminal replies instead of interpreting their APC/CSI payloads.
        const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
        self.mode
            .set_mode(self.mode.mode()? | ENABLE_VIRTUAL_TERMINAL_INPUT)
    }
}

#[cfg(windows)]
impl Drop for ConsoleInputMode {
    fn drop(&mut self) {
        let _ = self.mode.set_mode(self.original);
    }
}

#[cfg(windows)]
pub(crate) fn enable_terminal_replies() -> io::Result<ConsoleInputMode> {
    let mode = ConsoleInputMode::capture()?;
    mode.enable_vt()?;
    Ok(mode)
}

impl TerminalGuard {
    pub fn enter() -> anyhow::Result<Self> {
        execute!(
            stdout(),
            EnterAlternateScreen,
            cursor::Hide,
            crossterm::event::EnableMouseCapture,
        )
        .context("failed to enter alternate screen")?;
        enable_raw_mode().context("failed to enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        reset_terminal();
    }
}

/// Reset terminal state unconditionally (safe to call in panic hooks).
pub fn reset_terminal() {
    // Delete all Kitty graphics images so they don't linger after exit.
    // Raw escape: APC + "Ga=d" + ST  (action=delete, which=all)
    let _ = crate::kitty::write_kitty_packet(b"\x1b_Ga=d\x1b\\");

    let _ = disable_raw_mode();
    let _ = execute!(
        stdout(),
        LeaveAlternateScreen,
        cursor::Show,
        crossterm::event::DisableMouseCapture,
    );
}

// Used when neither the OS nor the terminal supplies pixel dimensions.
const DEFAULT_CELL_SIZE: (u16, u16) = (10, 20);
// A local terminal should answer immediately; unsupported queries must not hang startup.
const CELL_SIZE_TIMEOUT: Duration = Duration::from_millis(200);
const MAX_CELL_SIZE_REPLY: usize = 64;

/// Query terminal geometry, reusing cell dimensions during resize checks.
pub fn get_window_size(previous: Option<WindowSize>) -> anyhow::Result<WindowSize> {
    let (cols, rows) = crossterm::terminal::size().context("cannot query terminal cell count")?;
    // The Windows API exposes cell counts but crossterm's pixel query is unsupported.
    let pixels = window_size().ok().map(|ws| (ws.width, ws.height));
    resolve_window_size(cols, rows, pixels, || {
        previous
            .map(|ws| (ws.cell_width_px, ws.cell_height_px))
            .unwrap_or_else(|| query_cell_size().unwrap_or(DEFAULT_CELL_SIZE))
    })
}

fn resolve_window_size(
    cols: u16,
    rows: u16,
    pixels: Option<(u16, u16)>,
    fallback: impl FnOnce() -> (u16, u16),
) -> anyhow::Result<WindowSize> {
    anyhow::ensure!(cols > 0 && rows > 0, "terminal reported zero rows/cols");
    let (width_px, cell_width_px, cell_height_px) = if let Some((width, height)) =
        pixels.filter(|&(width, height)| width >= cols && height >= rows)
    {
        (width, width / cols, height / rows)
    } else {
        let (cell_w, cell_h) = fallback();
        anyhow::ensure!(cell_w > 0 && cell_h > 0, "terminal reported zero cell size");
        let width = cols
            .checked_mul(cell_w)
            .context("terminal pixel width overflow")?;
        (width, cell_w, cell_h)
    };

    Ok(WindowSize {
        rows,
        cols,
        width_px,
        cell_width_px,
        cell_height_px,
    })
}

fn query_cell_size() -> io::Result<(u16, u16)> {
    struct RestoreRawMode(bool);
    impl Drop for RestoreRawMode {
        fn drop(&mut self) {
            if self.0 {
                let _ = disable_raw_mode();
            }
        }
    }

    #[cfg(windows)]
    let _input_mode = enable_terminal_replies()?;
    let restore = RestoreRawMode(!is_raw_mode_enabled()?);
    if restore.0 {
        enable_raw_mode()?;
    }
    // CSI 16 t requests cell pixels; CSI 14 t requests the entire window's pixels.
    stdout().write_all(b"\x1b[16t")?;
    stdout().flush()?;
    let deadline = Instant::now() + CELL_SIZE_TIMEOUT;
    let mut reply = Vec::with_capacity(MAX_CELL_SIZE_REPLY);
    while reply.len() < MAX_CELL_SIZE_REPLY && Instant::now() < deadline {
        let Some(byte) = read_size_reply_byte(deadline)? else {
            break;
        };
        // The Windows event backend can omit the standalone ESC record.
        #[cfg(windows)]
        if reply.is_empty() && byte == b'[' {
            reply.push(0x1b);
        }
        reply.push(byte);
        if let Some(size) = parse_cell_size_reply(&reply) {
            return Ok(size);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "no valid terminal cell-size reply",
    ))
}

#[cfg(unix)]
fn read_size_reply_byte(deadline: Instant) -> io::Result<Option<u8>> {
    use nix::poll::{PollFd, PollFlags, poll};
    use std::os::fd::AsFd as _;

    let input = io::stdin();
    let mut fds = [PollFd::new(input.as_fd(), PollFlags::POLLIN)];
    let remaining = deadline.saturating_duration_since(Instant::now());
    let timeout = remaining.as_millis().min(u128::from(u16::MAX)) as u16;
    if poll(&mut fds, timeout)? == 0 {
        return Ok(None);
    }
    let mut byte = [0];
    // Read the descriptor directly so stdin's buffering cannot hide bytes from poll.
    Ok((nix::unistd::read(input.as_fd(), &mut byte)? > 0).then_some(byte[0]))
}

#[cfg(not(unix))]
fn read_size_reply_byte(deadline: Instant) -> io::Result<Option<u8>> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, poll, read};

    while Instant::now() < deadline {
        if !poll(deadline.saturating_duration_since(Instant::now()))? {
            break;
        }
        if let Event::Key(key) = read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Esc => return Ok(Some(0x1b)),
                KeyCode::Char(ch) if ch.is_ascii() => return Ok(Some(ch as u8)),
                _ => (),
            }
        }
    }
    Ok(None)
}

fn parse_cell_size_reply(reply: &[u8]) -> Option<(u16, u16)> {
    let text = std::str::from_utf8(reply).ok()?;
    let start = text.find("\x1b[6;")? + "\x1b[6;".len();
    let end = text[start..].find('t')? + start;
    let mut parts = text[start..end].split(';');
    let height = parts.next()?.parse::<u16>().ok()?;
    let width = parts.next()?.parse::<u16>().ok()?;
    (parts.next().is_none() && width > 0 && height > 0).then_some((width, height))
}

/// Draw the status bar on the last terminal row.
pub fn draw_status_bar(ws: &WindowSize, content: &str) -> anyhow::Result<()> {
    // Pad/truncate to fill the full row width.
    let padded = format!(
        " {:<width$}",
        content,
        width = (ws.cols as usize).saturating_sub(1)
    );
    execute!(
        stdout(),
        cursor::MoveTo(0, ws.rows.saturating_sub(1)),
        Print(padded),
    )?;
    Ok(())
}

/// Clear all terminal cells above the status bar.
pub fn clear_page_area(ws: &WindowSize) -> anyhow::Result<()> {
    for row in 0..ws.page_rows() {
        execute!(
            stdout(),
            cursor::MoveTo(0, row),
            Clear(ClearType::CurrentLine),
        )?;
    }
    Ok(())
}

/// Print "Loading..." centred in the page area.
pub fn draw_loading(ws: &WindowSize) -> anyhow::Result<()> {
    let msg = "Loading...";
    let x = ws.cols.saturating_sub(msg.len() as u16) / 2;
    let y = ws.page_rows() / 2;
    execute!(stdout(), cursor::MoveTo(x, y), Print(msg))?;
    Ok(())
}

/// Draw the table of contents overlay.
pub fn draw_toc(
    ws: &WindowSize,
    toc: &[crate::renderer::TocEntry],
    scroll: usize,
    current_page: usize,
) -> anyhow::Result<()> {
    let max_rows = ws.page_rows() as usize;
    let cols = ws.cols as usize;

    for (i, entry) in toc.iter().enumerate().skip(scroll).take(max_rows) {
        let row = (i - scroll) as u16;
        let indent = "  ".repeat(entry.level);
        let marker = if entry.page == current_page { ">" } else { " " };
        let highlight = i == scroll;

        let line = format!(
            "{marker}{indent}{title}  (p.{page})",
            title = entry.title,
            page = entry.page + 1,
        );

        // Truncate to terminal width
        let display: String = line.chars().take(cols).collect();

        if highlight {
            // Highlight selected entry with reverse video
            execute!(
                stdout(),
                cursor::MoveTo(0, row),
                crossterm::style::SetAttribute(crossterm::style::Attribute::Reverse),
                Print(format!("{:<width$}", display, width = cols)),
                crossterm::style::SetAttribute(crossterm::style::Attribute::Reset),
            )?;
        } else {
            execute!(stdout(), cursor::MoveTo(0, row), Print(display),)?;
        }
    }

    Ok(())
}

/// Draw the metadata overlay (full-screen text).
pub fn draw_metadata(ws: &WindowSize, metadata: &[(String, String)]) -> anyhow::Result<()> {
    let cols = ws.cols as usize;

    execute!(
        stdout(),
        cursor::MoveTo(0, 0),
        crossterm::style::SetAttribute(crossterm::style::Attribute::Reverse),
        Print(format!("{:<width$}", " Document Metadata", width = cols)),
        crossterm::style::SetAttribute(crossterm::style::Attribute::Reset),
    )?;

    for (i, (key, val)) in metadata.iter().enumerate() {
        let row = (i + 2) as u16;
        if row >= ws.page_rows() {
            break;
        }
        let line = format!("  {key}: {val}");
        let display: String = line.chars().take(cols).collect();
        execute!(stdout(), cursor::MoveTo(0, row), Print(display))?;
    }

    if metadata.is_empty() {
        execute!(
            stdout(),
            cursor::MoveTo(2, 2),
            Print("No metadata available."),
        )?;
    }

    Ok(())
}

/// Draw the links overlay (numbered list).
pub fn draw_links(
    ws: &WindowSize,
    links: &[crate::renderer::LinkInfo],
    input: &str,
    page_num: usize,
) -> anyhow::Result<()> {
    let cols = ws.cols as usize;

    let title = format!(" Links on page {} ({} found)", page_num + 1, links.len());
    execute!(
        stdout(),
        cursor::MoveTo(0, 0),
        crossterm::style::SetAttribute(crossterm::style::Attribute::Reverse),
        Print(format!("{:<width$}", title, width = cols)),
        crossterm::style::SetAttribute(crossterm::style::Attribute::Reset),
    )?;

    if links.is_empty() {
        execute!(
            stdout(),
            cursor::MoveTo(2, 2),
            Print("No links on this page."),
        )?;
    } else {
        for (i, link) in links.iter().enumerate() {
            let row = (i + 2) as u16;
            if row >= ws.page_rows().saturating_sub(1) {
                break;
            }
            let line = format!("  {}: {}", i + 1, link.text);
            let display: String = line.chars().take(cols).collect();
            execute!(stdout(), cursor::MoveTo(0, row), Print(display))?;
        }
    }

    let prompt_row = ws.page_rows().saturating_sub(1);
    let prompt = format!("Enter number to follow (Esc to cancel): {input}");
    execute!(stdout(), cursor::MoveTo(0, prompt_row), Print(prompt))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_pixel_query_uses_reported_cell_size() {
        let size = resolve_window_size(80, 24, None, || (9, 19)).unwrap();
        assert_eq!(size.width_px, 720);
        assert_eq!(size.cell_width_px, 9);
        assert_eq!(size.cell_height_px, 19);
        assert_eq!(size.page_area_height_px(), 437.0);
    }

    #[test]
    fn native_pixel_size_does_not_read_terminal_input() {
        let size = resolve_window_size(80, 24, Some((720, 456)), || {
            panic!("native geometry must not trigger a terminal query");
        })
        .unwrap();
        assert_eq!((size.cell_width_px, size.cell_height_px), (9, 19));
    }

    #[test]
    fn missing_pixel_size_can_use_default_cells() {
        for pixels in [None, Some((0, 0)), Some((1, 1))] {
            let size = resolve_window_size(80, 24, pixels, || DEFAULT_CELL_SIZE).unwrap();
            assert_eq!(size.width_px, 800);
            assert_eq!(size.cell_height_px, 20);
        }
    }

    #[test]
    fn invalid_geometry_is_rejected_before_arithmetic() {
        assert!(resolve_window_size(0, 24, None, || DEFAULT_CELL_SIZE).is_err());
        assert!(resolve_window_size(80, 0, None, || DEFAULT_CELL_SIZE).is_err());
        assert!(resolve_window_size(80, 24, None, || (0, 20)).is_err());
        assert!(resolve_window_size(u16::MAX, 24, None, || DEFAULT_CELL_SIZE).is_err());
    }

    #[test]
    fn parses_cell_pixels_in_width_height_order() {
        assert_eq!(parse_cell_size_reply(b"\x1b[6;19;9t"), Some((9, 19)));
        assert_eq!(parse_cell_size_reply(b"noise\x1b[6;20;10t"), Some((10, 20)));
    }

    #[test]
    fn rejects_window_pixels_and_invalid_cell_reports() {
        for reply in [
            b"\x1b[4;456;720t".as_slice(),
            b"\x1b[6;19;9",
            b"\x1b[6;0;9t",
            b"\x1b[6;19;0t",
            b"\x1b[6;19;65536t",
            b"\x1b[6;19;9;1t",
            b"\x1b[6;a;9t",
        ] {
            assert_eq!(parse_cell_size_reply(reply), None, "reply: {reply:?}");
        }
    }

    fn ws(rows: u16, cols: u16, cell_w: u16, cell_h: u16) -> WindowSize {
        WindowSize {
            rows,
            cols,
            width_px: cols * cell_w,
            cell_width_px: cell_w,
            cell_height_px: cell_h,
        }
    }

    #[test]
    fn page_area_width_is_full_terminal() {
        let w = ws(24, 80, 10, 20);
        assert!((w.page_area_width_px() - 800.0).abs() < f32::EPSILON);
    }

    #[test]
    fn page_area_height_excludes_status_bar() {
        let w = ws(25, 80, 10, 20);
        // (25-1) * 20 = 480
        assert!((w.page_area_height_px() - 480.0).abs() < f32::EPSILON);
    }

    #[test]
    fn page_rows_excludes_status_bar() {
        let w = ws(25, 80, 10, 20);
        assert_eq!(w.page_rows(), 24);
    }

    #[test]
    fn page_rows_saturates_at_zero() {
        let w = ws(0, 80, 10, 20);
        assert_eq!(w.page_rows(), 0);
    }

    #[test]
    fn page_area_single_row() {
        let w = ws(1, 80, 10, 20);
        // page_rows = 0, page_area_height = 0
        assert_eq!(w.page_rows(), 0);
        assert!((w.page_area_height_px() - 0.0).abs() < f32::EPSILON);
    }
}
