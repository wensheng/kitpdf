// Kitty graphics protocol helpers.

use std::io::{self, Write};
use std::num::NonZeroU32;
use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::Duration;

use crossterm::{cursor::MoveTo, event::EventStream, execute};
use image::DynamicImage;
use kittage::{
    AsyncInputReader, IdentifierType, ImageDimensions, ImageId, NumberOrId, PixelFormat,
    action::Action,
    delete::{ClearOrDelete, DeleteConfig, WhichToDelete},
    display::{CursorMovementPolicy, DisplayConfig, DisplayLocation},
    error::{ParseError, TransmitError},
    event_stream::InputErr,
    image::Image,
    medium::Medium,
    tmux::TmuxWriter,
};

use crate::image_pipeline::MaybeTransferred;

const KITTY_PLACEHOLDER_CODEPOINT: u32 = 0x10EEEE;
const DIACRITIC_CODEPOINTS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

static TMUX_PASSTHROUGH_INIT: Once = Once::new();

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Terminal cursor position in cells.
#[derive(Debug, Clone, Copy)]
pub struct Pos {
    pub x: u16,
    pub y: u16,
}

pub struct KittyReadyToDisplay<'a> {
    pub img: &'a mut MaybeTransferred,
    pub page_num: usize,
    pub pos: Pos,
    pub display_loc: DisplayLocation,
}

pub enum KittyDisplay<'a> {
    DisplayImages(Vec<KittyReadyToDisplay<'a>>),
}

// ---------------------------------------------------------------------------
// Debug writer (logs outgoing bytes in debug builds)
// ---------------------------------------------------------------------------

struct DbgWriter<W: Write> {
    w: W,
    #[cfg(debug_assertions)]
    buf: String,
}

impl<W: std::io::Write> std::io::Write for DbgWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        #[cfg(debug_assertions)]
        if let Ok(s) = std::str::from_utf8(buf) {
            self.buf.push_str(s);
        }
        self.w.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        #[cfg(debug_assertions)]
        {
            log::debug!("kitty write: {:?}", self.buf);
            self.buf.clear();
        }
        self.w.flush()
    }
}

// ---------------------------------------------------------------------------
// Timeout wrapper
// ---------------------------------------------------------------------------

/// kittage hardcodes a 1-second timeout for reading the terminal's response
/// after transmitting an image. On Linux, large images sent via the Direct
/// medium can take longer than that to process, producing a spurious
/// "deadline has elapsed" error. This wrapper enforces a minimum timeout.
struct MinTimeoutStream<'a> {
    inner: &'a mut EventStream,
    min_timeout: Duration,
}

/// Minimum time we are willing to wait for the terminal to acknowledge an
/// image transmission.  5 s is generous enough for large pages while still
/// surfacing genuine failures quickly.
const MIN_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

impl AsyncInputReader for &mut MinTimeoutStream<'_> {
    type Error = InputErr;

    async fn read_esc_delimited_str_with_timeout(
        &mut self,
        buf: &mut String,
        timeout: Duration,
    ) -> Result<(), Self::Error> {
        let effective = timeout.max(self.min_timeout);
        self.inner
            .read_esc_delimited_str_with_timeout(buf, effective)
            .await
    }
}

// ---------------------------------------------------------------------------
// tmux passthrough and Unicode placeholders
// ---------------------------------------------------------------------------

pub fn tmux_passthrough_needed() -> bool {
    let Some(tmux_env) = std::env::var_os("TMUX") else {
        return false;
    };
    if tmux_env.as_os_str().is_empty() {
        return false;
    }

    TMUX_PASSTHROUGH_INIT.call_once(|| {
        let _ = Command::new("tmux")
            .args(["set", "-p", "allow-passthrough", "on"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });

    true
}

fn tmux_placeholder_cells(cols: u16, rows: u16) -> (u16, u16) {
    let max = DIACRITIC_CODEPOINTS.len() as u16;
    (cols.clamp(1, max), rows.clamp(1, max))
}

fn append_codepoint(buf: &mut Vec<u8>, codepoint: u32) {
    if let Some(ch) = char::from_u32(codepoint) {
        let mut encoded = [0; 4];
        buf.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
    }
}

fn append_diacritic(buf: &mut Vec<u8>, value: u16) {
    if let Some(codepoint) = DIACRITIC_CODEPOINTS.get(value as usize) {
        append_codepoint(buf, *codepoint);
    }
}

fn append_tmux_image_color(buf: &mut Vec<u8>, sgr_key: u8, id: ImageId) -> io::Result<()> {
    let id = id.get();
    write!(
        buf,
        "\x1b[{sgr_key}:2:{}:{}:{}m",
        (id >> 16) & 0xff,
        (id >> 8) & 0xff,
        id & 0xff
    )
}

fn append_tmux_placeholder_colors(
    buf: &mut Vec<u8>,
    image_id: ImageId,
    placement_id: Option<ImageId>,
) -> io::Result<()> {
    append_tmux_image_color(buf, 38, image_id)?;
    if let Some(placement_id) = placement_id {
        append_tmux_image_color(buf, 58, placement_id)?;
    }
    Ok(())
}

fn append_tmux_placeholders(
    buf: &mut Vec<u8>,
    image_id: ImageId,
    placement_id: Option<ImageId>,
    pos: Pos,
    cols: u16,
    rows: u16,
    restore_cursor: bool,
) -> io::Result<()> {
    let (cols, rows) = tmux_placeholder_cells(cols, rows);
    if restore_cursor {
        buf.extend_from_slice(b"\x1b7");
    }

    write!(buf, "\x1b[{};1H", u32::from(pos.y) + 1)?;
    append_tmux_placeholder_colors(buf, image_id, placement_id)?;

    for row in 0..rows {
        if pos.x > 0 {
            write!(buf, "\x1b[{}C", pos.x)?;
        }
        for col in 0..cols {
            append_codepoint(buf, KITTY_PLACEHOLDER_CODEPOINT);
            append_diacritic(buf, row);
            append_diacritic(buf, col);
        }
        if row + 1 < rows {
            buf.extend_from_slice(b"\x1b[39m");
            if placement_id.is_some() {
                buf.extend_from_slice(b"\x1b[59m");
            }
            buf.extend_from_slice(b"\n\r");
            append_tmux_placeholder_colors(buf, image_id, placement_id)?;
        }
    }

    buf.extend_from_slice(b"\x1b[39m");
    if placement_id.is_some() {
        buf.extend_from_slice(b"\x1b[59m");
    }
    if restore_cursor {
        buf.extend_from_slice(b"\x1b8");
    }
    Ok(())
}

fn display_config_for(display_loc: DisplayLocation, tmux_passthrough: bool) -> DisplayConfig {
    let mut location = display_loc;
    if tmux_passthrough {
        let (cols, rows) = tmux_placeholder_cells(location.columns, location.rows);
        location.columns = cols;
        location.rows = rows;
    }

    DisplayConfig {
        location,
        cursor_movement: CursorMovementPolicy::DontMove,
        create_virtual_placement: tmux_passthrough,
        ..DisplayConfig::default()
    }
}

fn tmux_placement_id_for_image(image: &Image<'_>) -> Option<ImageId> {
    match image.num_or_id {
        NumberOrId::Id(id) => Some(id),
        NumberOrId::Number(_) => None,
    }
}

fn write_tmux_placeholders(
    image_id: ImageId,
    placement_id: Option<ImageId>,
    pos: Pos,
    cols: u16,
    rows: u16,
) -> Result<(), TransmitError<InputErr>> {
    let mut placeholders = Vec::new();
    append_tmux_placeholders(
        &mut placeholders,
        image_id,
        placement_id,
        pos,
        cols,
        rows,
        true,
    )
    .map_err(TransmitError::Writing)?;

    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&placeholders)
        .and_then(|_| stdout.flush())
        .map_err(TransmitError::Writing)
}

pub fn write_kitty_packet(packet: &[u8]) -> io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    if tmux_passthrough_needed() {
        let mut writer = TmuxWriter::new(&mut stdout);
        writer.write_all(packet)?;
        writer.flush()
    } else {
        stdout.write_all(packet)?;
        stdout.flush()
    }
}

// ---------------------------------------------------------------------------
// Core async helpers
// ---------------------------------------------------------------------------

/// Execute a single kittage action and await the terminal's response.
pub async fn run_action(
    action: Action<'_, '_>,
    ev_stream: &mut EventStream,
) -> Result<Option<ImageId>, TransmitError<InputErr>> {
    if tmux_passthrough_needed() {
        let writer = DbgWriter {
            w: TmuxWriter::new(std::io::stdout().lock()),
            #[cfg(debug_assertions)]
            buf: String::new(),
        };
        return run_action_with_writer(action, writer, ev_stream).await;
    }

    let writer = DbgWriter {
        w: std::io::stdout().lock(),
        #[cfg(debug_assertions)]
        buf: String::new(),
    };
    run_action_with_writer(action, writer, ev_stream).await
}

async fn run_action_with_writer<W: Write>(
    action: Action<'_, '_>,
    writer: W,
    ev_stream: &mut EventStream,
) -> Result<Option<ImageId>, TransmitError<InputErr>> {
    let mut reader = MinTimeoutStream {
        inner: ev_stream,
        min_timeout: MIN_RESPONSE_TIMEOUT,
    };
    action
        .execute_async(writer, &mut reader)
        .await
        .map(|(_, id)| id)
}

fn missing_image_id_error() -> TransmitError<InputErr> {
    TransmitError::ParsingResponse(ParseError::NoResponseId {
        ty: IdentifierType::ImageId,
    })
}

/// Test whether the terminal supports the Kitty graphics protocol at all.
pub async fn supports_kitty_graphics(ev_stream: &mut EventStream) -> bool {
    let mut img: Image<'static> = DynamicImage::new_rgb8(1, 1).into();
    img.num_or_id = NumberOrId::Id(NonZeroU32::new(u32::MAX - 1).unwrap());
    run_action(Action::Query(&img), ev_stream).await.is_ok()
}

/// Test whether shared-memory image transfer works in this terminal.
/// Returns `true` if SHM is supported (Kitty/Ghostty on Linux/macOS).
pub async fn do_shms_work(ev_stream: &mut EventStream) -> bool {
    let img = DynamicImage::new_rgb8(1, 1);
    let pid = std::process::id();
    let shm_name = format!("kitpdf_test_{pid}");

    #[cfg(unix)]
    let shm_name = &*shm_name;

    let Ok(mut k_img) = kittage::image::Image::shm_from(img, shm_name) else {
        return false;
    };

    k_img.num_or_id = NumberOrId::Id(NonZeroU32::new(u32::MAX).unwrap());

    // Raw mode must already be enabled by TerminalGuard before calling this.
    let res = run_action(Action::Query(&k_img), ev_stream).await;
    res.is_ok()
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

/// Re-display a set of Kitty images.
///
/// On error, returns the page numbers that failed so the main loop can
/// trigger a re-render for them.
pub async fn display_kitty_images(
    display: KittyDisplay<'_>,
    ev_stream: &mut EventStream,
) -> Result<(), (Vec<usize>, &'static str, TransmitError<InputErr>)> {
    let KittyDisplay::DisplayImages(images) = display;

    let mut err: Option<(Vec<usize>, TransmitError<_>)> = None;

    for KittyReadyToDisplay {
        img,
        page_num,
        pos,
        display_loc,
    } in images
    {
        let tmux_passthrough = tmux_passthrough_needed();
        let config = display_config_for(display_loc, tmux_passthrough);
        let placeholder_cols = config.location.columns;
        let placeholder_rows = config.location.rows;

        execute!(std::io::stdout(), MoveTo(pos.x, pos.y)).unwrap();

        log::debug!(
            "displaying page {page_num} at {:?}, loc {:?}",
            pos,
            config.location
        );

        let this_err = match img {
            MaybeTransferred::NotYet(image) => {
                // Swap out the real image so we can move it into the action.
                let mut placeholder = Image {
                    num_or_id: image.num_or_id,
                    format: PixelFormat::Rgb24(
                        ImageDimensions {
                            width: 0,
                            height: 0,
                        },
                        None,
                    ),
                    medium: Medium::Direct {
                        chunk_size: None,
                        data: (&[]).into(),
                    },
                };
                std::mem::swap(image, &mut placeholder);

                let placement_id = tmux_passthrough
                    .then(|| tmux_placement_id_for_image(&placeholder))
                    .flatten();

                match run_action(
                    Action::TransmitAndDisplay {
                        image: placeholder,
                        config,
                        placement_id,
                    },
                    ev_stream,
                )
                .await
                {
                    Ok(Some(img_id)) => {
                        *img = MaybeTransferred::Transferred(img_id);
                        if tmux_passthrough {
                            write_tmux_placeholders(
                                img_id,
                                placement_id,
                                pos,
                                placeholder_cols,
                                placeholder_rows,
                            )
                            .map_err(|e| (page_num, e))
                        } else {
                            Ok(())
                        }
                    }
                    Ok(None) => Err((page_num, missing_image_id_error())),
                    Err(e) => Err((page_num, e)),
                }
            }
            MaybeTransferred::Transferred(image_id) => {
                let placement_id = *image_id;
                run_action(
                    Action::Display {
                        image_id: *image_id,
                        placement_id,
                        config,
                    },
                    ev_stream,
                )
                .await
                .and_then(|_| {
                    if tmux_passthrough {
                        write_tmux_placeholders(
                            *image_id,
                            Some(placement_id),
                            pos,
                            placeholder_cols,
                            placeholder_rows,
                        )?;
                    }
                    Ok(())
                })
                .map_err(|e| (page_num, e))
            }
        };

        if let Err((id, e)) = this_err {
            let entry = err.get_or_insert_with(|| (vec![], e));
            entry.0.push(id);
        }
    }

    match err {
        Some((pages, e)) => Err((pages, "couldn't transfer image to terminal", e)),
        None => Ok(()),
    }
}

pub async fn clear_kitty_image(
    image_id: ImageId,
    ev_stream: &mut EventStream,
) -> Result<(), TransmitError<InputErr>> {
    run_action(
        Action::Delete(DeleteConfig {
            effect: ClearOrDelete::Clear,
            which: WhichToDelete::ImageId(image_id, None),
        }),
        ev_stream,
    )
    .await
    .map(|_| ())
}

pub async fn delete_kitty_images(
    image_ids: Vec<ImageId>,
    ev_stream: &mut EventStream,
) -> Result<(), TransmitError<InputErr>> {
    for image_id in image_ids {
        run_action(
            Action::Delete(DeleteConfig {
                effect: ClearOrDelete::Delete,
                which: WhichToDelete::ImageId(image_id, None),
            }),
            ev_stream,
        )
        .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_placeholder_cells_are_capped_to_available_diacritics() {
        assert_eq!(tmux_placeholder_cells(999, 999), (297, 297));
        assert_eq!(tmux_placeholder_cells(0, 0), (1, 1));
    }

    #[test]
    fn tmux_display_config_uses_virtual_placement_and_capped_cells() {
        let loc = DisplayLocation {
            columns: 999,
            rows: 0,
            ..DisplayLocation::default()
        };

        let config = display_config_for(loc.clone(), true);
        assert_eq!(config.location.columns, 297);
        assert_eq!(config.location.rows, 1);
        assert_eq!(config.cursor_movement, CursorMovementPolicy::DontMove);
        assert!(config.create_virtual_placement);

        let direct = display_config_for(loc, false);
        assert_eq!(direct.location.columns, 999);
        assert_eq!(direct.location.rows, 0);
        assert_eq!(direct.cursor_movement, CursorMovementPolicy::DontMove);
        assert!(!direct.create_virtual_placement);
    }

    #[test]
    fn tmux_placeholders_encode_image_placement_and_cell_coordinates() {
        let image_id = NonZeroU32::new(0x00010203).unwrap();
        let placement_id = NonZeroU32::new(0x00040506).unwrap();
        let mut placeholders = Vec::new();

        append_tmux_placeholders(
            &mut placeholders,
            image_id,
            Some(placement_id),
            Pos { x: 3, y: 4 },
            2,
            2,
            true,
        )
        .unwrap();

        let placeholders = String::from_utf8(placeholders).unwrap();
        let placeholder = char::from_u32(KITTY_PLACEHOLDER_CODEPOINT).unwrap();
        let row_col_zero = char::from_u32(DIACRITIC_CODEPOINTS[0]).unwrap();
        let row_col_one = char::from_u32(DIACRITIC_CODEPOINTS[1]).unwrap();

        assert!(placeholders.starts_with("\x1b7\x1b[5;1H\x1b[38:2:1:2:3m\x1b[58:2:4:5:6m\x1b[3C"));
        assert!(placeholders.contains(&format!("{placeholder}{row_col_zero}{row_col_zero}")));
        assert!(placeholders.contains(&format!("{placeholder}{row_col_one}{row_col_one}")));
        assert!(placeholders.ends_with("\x1b[39m\x1b[59m\x1b8"));
    }
}
