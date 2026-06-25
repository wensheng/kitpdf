// Kitty graphics protocol helpers.
//
// The low-level protocol types in this file are adapted from kittage 0.4.0
// (MPL-2.0) so kitpdf does not need to depend on the external crate.

use std::error::Error;
use std::fmt::Display;
use std::io::{self, Write};
use std::num::NonZeroU32;
use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::Duration;

use ::image::DynamicImage;
use crossterm::{cursor::MoveTo, event::EventStream, execute};

use crate::image_pipeline::MaybeTransferred;

pub type ImageId = NonZeroU32;
pub type PlacementId = NonZeroU32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumberOrId {
    Number(NonZeroU32),
    Id(NonZeroU32),
}

#[derive(Debug, PartialEq)]
pub enum IdentifierType {
    ImageId,
    ImageNumber,
    PlacementId,
}

#[derive(PartialEq, Debug)]
pub enum AnyValueOrSpecific<T> {
    Any,
    Specific(T),
}

impl<T: Display> Display for AnyValueOrSpecific<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Any => write!(f, "<any value>"),
            Self::Specific(val) => write!(f, "{val}"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Hash, Debug)]
enum Verbosity {
    All = 0,
}

pub trait AsyncInputReader {
    type Error: Error;

    fn read_esc_delimited_str_with_timeout(
        &mut self,
        buf: &mut String,
        timeout: Duration,
    ) -> impl Future<Output = Result<(), Self::Error>>;
}

fn write_param<W: Write, T: Display + PartialEq>(
    mut writer: W,
    key: char,
    value: T,
    default: T,
) -> io::Result<W> {
    if value != default {
        write!(writer, ",{key}={value}")?;
    }
    Ok(writer)
}

#[derive(PartialEq, Clone, Debug)]
pub enum PixelFormat {
    Rgb24(ImageDimensions),
    Rgba32(ImageDimensions),
}

impl Default for PixelFormat {
    fn default() -> Self {
        Self::Rgb24(ImageDimensions {
            width: 0,
            height: 0,
        })
    }
}

impl PixelFormat {
    fn write_to<W: Write>(&self, mut writer: W) -> io::Result<W> {
        if self == &Self::default() {
            return Ok(writer);
        }

        fn write_dimensions<W: Write>(writer: W, width: u32, height: u32) -> io::Result<W> {
            let writer = write_param(writer, 's', width, 0)?;
            write_param(writer, 'v', height, 0)
        }

        write!(writer, ",f=")?;
        match self {
            Self::Rgb24(ImageDimensions { width, height }) => {
                write!(writer, "24")?;
                write_dimensions(writer, *width, *height)
            }
            Self::Rgba32(ImageDimensions { width, height }) => {
                write!(writer, "32")?;
                write_dimensions(writer, *width, *height)
            }
        }
    }
}

#[derive(PartialEq, Clone, Debug)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

pub mod error {
    use std::{error::Error, num::NonZeroU32};

    use super::{AnyValueOrSpecific, IdentifierType};

    #[derive(thiserror::Error, Debug, PartialEq)]
    pub enum ParseError {
        #[error("Terminal Response didn't start with '\\x1b_G' but instead '{0}'")]
        NoStartSequence(String),
        #[error(
            "No final semicolon was provided to indicate end of options in the terminal response"
        )]
        NoFinalSemicolon,
        #[error(
            "An ID of type {ty:?} was sent, but no relevant ID was seen in the terminal response"
        )]
        NoResponseId { ty: IdentifierType },
        #[error(
            "The ID of type {ty:?} seen in the terminal response was different from the one we sent (was {found}, expected {expected})"
        )]
        DifferentIdInResponse {
            ty: IdentifierType,
            found: String,
            expected: AnyValueOrSpecific<NonZeroU32>,
        },
        #[error(
            "The terminal response provided a id of type {ty:?}, but we didn't sent one as part of the request (with a value of '{value}')"
        )]
        IdInResponseButNotInRequest { ty: IdentifierType, value: String },
        #[error(
            "We expected to receive an ID of type {ty:?} with value {val} in the response, but saw no id of that type"
        )]
        MissingId {
            ty: IdentifierType,
            val: AnyValueOrSpecific<NonZeroU32>,
        },
        #[error("Unknown terminal response key '{0}'")]
        UnknownResponseKey(String),
        #[error(
            "The terminal returned an error, but it was unparseable; was '{0}' but expected <code>:<reason>"
        )]
        MalformedError(String),
        #[error(
            "The terminal resported an error with an unknown error code '{code}' and accompanying reason '{reason}'"
        )]
        UnknownErrorCode { code: String, reason: String },
    }

    #[non_exhaustive]
    #[derive(thiserror::Error, Debug, PartialEq)]
    pub enum TerminalError {
        #[error("No such entity was found: {0}")]
        NoEntity(String),
        #[error("Invalid argument: {0}")]
        InvalidArgument(String),
        #[error("Bad file: {0}")]
        BadFile(String),
        #[error("No Data: {0}")]
        NoData(String),
        #[error("File Too Large: {0}")]
        FileTooLarge(String),
    }

    pub struct UnknownErrorCode<'a>(pub(crate) &'a str);

    impl<'a> TryFrom<(&'a str, &str)> for TerminalError {
        type Error = UnknownErrorCode<'a>;

        fn try_from(value: (&'a str, &str)) -> Result<Self, Self::Error> {
            let reason = value.1.to_owned();
            Ok(match value.0 {
                "ENOENT" => Self::NoEntity(reason),
                "EINVAL" => Self::InvalidArgument(reason),
                "EBADF" => Self::BadFile(reason),
                "ENODATA" => Self::NoData(reason),
                "EFBIG" => Self::FileTooLarge(reason),
                code => return Err(UnknownErrorCode(code)),
            })
        }
    }

    #[derive(thiserror::Error, Debug)]
    pub enum TransmitError<InputError: Error> {
        #[error("Couldn't write to writer: {0}")]
        Writing(std::io::Error),
        #[error("Couldn't read input after transmitting: {0}")]
        ReadingInput(InputError),
        #[error("Couldn't parse response from the terminal: {0}")]
        ParsingResponse(ParseError),
        #[error("The backing terminal returned an error: {0}")]
        Terminal(TerminalError),
    }

    impl<E: PartialEq + Error> PartialEq for TransmitError<E> {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (Self::Writing(e1), Self::Writing(e2)) => e1.to_string() == e2.to_string(),
                (Self::ReadingInput(e1), Self::ReadingInput(e2)) => e1 == e2,
                (Self::ParsingResponse(e1), Self::ParsingResponse(e2)) => e1 == e2,
                (Self::Terminal(e1), Self::Terminal(e2)) => e1 == e2,
                _ => false,
            }
        }
    }
}

pub mod event_stream {
    use std::{
        pin::Pin,
        task::{Context, Poll},
        time::Duration,
    };

    use crossterm::event::{Event, EventStream, KeyModifiers};
    use futures_util::Stream;

    use super::AsyncInputReader;

    #[derive(Debug, thiserror::Error)]
    pub enum InputErr {
        #[error("{0}")]
        Timeout(#[from] tokio::time::error::Elapsed),
        #[error("{0}")]
        IO(#[from] std::io::Error),
    }

    impl PartialEq for InputErr {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (Self::Timeout(e1), Self::Timeout(e2)) => e1 == e2,
                (Self::IO(e1), Self::IO(e2)) => e1.kind() == e2.kind(),
                _ => false,
            }
        }
    }

    impl AsyncInputReader for &mut EventStream {
        type Error = InputErr;

        async fn read_esc_delimited_str_with_timeout(
            &mut self,
            buf: &mut String,
            timeout: Duration,
        ) -> Result<(), Self::Error> {
            tokio::time::timeout(timeout, async {
                let mut found_one_esc = false;
                loop {
                    match Next(Some(*self)).await {
                        None => return Ok(()),
                        Some(Err(e)) => return Err(e),
                        Some(Ok(Event::Key(k))) => {
                            if let Some(c) = k.code.as_char() {
                                if k.modifiers.contains(KeyModifiers::ALT) {
                                    if !found_one_esc {
                                        found_one_esc = true;
                                    } else {
                                        return Ok(());
                                    }
                                }

                                buf.push(c);
                            }
                        }
                        Some(Ok(Event::Paste(s))) => match s.find('\x1b') {
                            Some(pos) => {
                                buf.push_str(&s[..pos]);
                                return Ok(());
                            }
                            None => buf.push_str(&s),
                        },
                        Some(Ok(_)) => (),
                    }
                }
            })
            .await?
            .map_err(InputErr::from)
        }
    }

    struct Next<'a>(Option<&'a mut EventStream>);

    impl Future for Next<'_> {
        type Output = Option<<EventStream as Stream>::Item>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match &mut self.0 {
                Some(stream) => match Pin::<&mut EventStream>::new(stream).poll_next(cx) {
                    Poll::Ready(item) => {
                        self.0 = None;
                        Poll::Ready(item)
                    }
                    Poll::Pending => Poll::Pending,
                },
                None => Poll::Pending,
            }
        }
    }

    impl Unpin for Next<'_> where EventStream: Unpin {}
}

pub mod medium {
    use std::{borrow::Cow, io::Write, num::NonZeroU16};

    #[derive(Debug, PartialEq)]
    pub struct ChunkSize(NonZeroU16);

    impl Default for ChunkSize {
        fn default() -> Self {
            Self(NonZeroU16::new(1024).unwrap())
        }
    }

    #[derive(Debug, PartialEq)]
    pub enum Medium<'data> {
        Direct {
            chunk_size: Option<ChunkSize>,
            data: Cow<'data, [u8]>,
        },
        SharedMemObject(SharedMemObject),
    }

    #[cfg(unix)]
    #[derive(Debug)]
    struct UnixShm {
        shm: psx_shm::UnlinkOnDrop,
        map: memmap2::MmapMut,
    }

    #[derive(Debug)]
    pub struct SharedMemObject {
        #[cfg(unix)]
        inner: UnixShm,
    }

    impl PartialEq for SharedMemObject {
        fn eq(&self, other: &Self) -> bool {
            self.name() == other.name()
        }
    }

    #[cfg(unix)]
    #[derive(Debug)]
    pub struct ShmError {
        pub step: ShmCreationFailureStep,
        pub err: std::io::Error,
    }

    #[cfg(unix)]
    impl std::fmt::Display for ShmError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.step {
                ShmCreationFailureStep::ShmOpen => write!(f, "Couldn't open SHM: {}", self.err),
                ShmCreationFailureStep::ShmSetSize => {
                    write!(f, "Couldn't set the size of the SHM: {}", self.err)
                }
                ShmCreationFailureStep::MemmapCreation => {
                    write!(
                        f,
                        "Couldn't create the memmap necessary to write to this SHM: {}",
                        self.err
                    )
                }
            }
        }
    }

    #[cfg(unix)]
    impl std::error::Error for ShmError {}

    #[cfg(unix)]
    #[derive(Debug)]
    pub enum ShmCreationFailureStep {
        ShmOpen,
        ShmSetSize,
        MemmapCreation,
    }

    impl SharedMemObject {
        #[cfg(unix)]
        pub fn create_new(name: &str, size: usize) -> Result<Self, ShmError> {
            use psx_shm::UnlinkOnDrop;
            use rustix::{fs::Mode, shm::OFlags};

            let mut shm = psx_shm::Shm::open(
                name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|err| ShmError {
                step: ShmCreationFailureStep::ShmOpen,
                err,
            })?;

            shm.set_size(size).map_err(|err| ShmError {
                step: ShmCreationFailureStep::ShmSetSize,
                err,
            })?;

            let borrowed_mmap = unsafe { shm.map(0) }.map_err(|err| ShmError {
                step: ShmCreationFailureStep::MemmapCreation,
                err,
            })?;
            let map = unsafe { borrowed_mmap.into_map() };

            Ok(Self {
                inner: UnixShm {
                    shm: UnlinkOnDrop { shm },
                    map,
                },
            })
        }

        fn name(&self) -> Box<str> {
            #[cfg(unix)]
            {
                self.inner.shm.shm.name().into()
            }
            #[cfg(not(unix))]
            {
                unreachable!("shared memory transfer is only implemented on unix")
            }
        }

        pub fn copy_in_buf(&mut self, buf: &[u8]) -> std::io::Result<()> {
            #[cfg(unix)]
            {
                use std::cmp::Ordering;

                match buf.len().cmp(&self.inner.map.as_ref().len()) {
                    Ordering::Less => {
                        self.inner.map[..buf.len()].copy_from_slice(buf);
                        for byte in &mut self.inner.map[buf.len()..] {
                            *byte = 0;
                        }
                    }
                    Ordering::Equal => self.inner.map.copy_from_slice(buf),
                    Ordering::Greater => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "Provided buffer is too large to fit",
                        ));
                    }
                }

                Ok(())
            }
            #[cfg(not(unix))]
            {
                let _ = buf;
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "shared memory transfer is only implemented on unix",
                ))
            }
        }
    }

    fn write_b64<W: Write>(data: &[u8], mut writer: W) -> std::io::Result<W> {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

        let mut encoded = Vec::with_capacity(data.len().div_ceil(3) * 4);
        let mut chunks = data.chunks_exact(3);
        for chunk in &mut chunks {
            let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
            encoded.push(TABLE[((n >> 18) & 0x3f) as usize]);
            encoded.push(TABLE[((n >> 12) & 0x3f) as usize]);
            encoded.push(TABLE[((n >> 6) & 0x3f) as usize]);
            encoded.push(TABLE[(n & 0x3f) as usize]);
        }

        match chunks.remainder() {
            [a] => {
                let n = u32::from(*a) << 16;
                encoded.push(TABLE[((n >> 18) & 0x3f) as usize]);
                encoded.push(TABLE[((n >> 12) & 0x3f) as usize]);
            }
            [a, b] => {
                let n = (u32::from(*a) << 16) | (u32::from(*b) << 8);
                encoded.push(TABLE[((n >> 18) & 0x3f) as usize]);
                encoded.push(TABLE[((n >> 12) & 0x3f) as usize]);
                encoded.push(TABLE[((n >> 6) & 0x3f) as usize]);
            }
            [] => {}
            _ => unreachable!(),
        }

        writer.write_all(&encoded)?;
        Ok(writer)
    }

    impl Medium<'_> {
        pub(crate) fn write_data<W: Write>(&self, mut writer: W) -> std::io::Result<W> {
            let name: Box<str>;
            let (key, data) = match self {
                Self::Direct { data, chunk_size } => {
                    if let Some(chunk_size) = chunk_size {
                        write!(writer, ",t=d,")?;
                        let encoded_bytes_per_chunk = usize::from(chunk_size.0.get()) * 4;
                        let bytes_per_chunk = (encoded_bytes_per_chunk / 4) * 3;
                        let total_chunks = data.len().div_ceil(bytes_per_chunk);
                        let mut chunks = data.chunks(bytes_per_chunk);

                        if let Some(first) = chunks.next() {
                            write!(writer, "m={};", u8::from(total_chunks != 1))?;
                            writer = write_b64(first, writer)?;
                            write!(writer, "\x1b\\")?;
                        }

                        for (idx, chunk) in chunks.enumerate() {
                            write!(writer, "\x1b_Gm={};", u8::from(total_chunks != idx + 2))?;
                            writer = write_b64(chunk, writer)?;
                            write!(writer, "\x1b\\")?;
                        }

                        return Ok(writer);
                    }

                    ('d', &**data)
                }
                Self::SharedMemObject(obj) => {
                    name = obj.name();
                    ('s', name.as_bytes())
                }
            };

            write!(writer, ",t={key};")?;
            write_b64(data, writer)
        }
    }
}

pub mod display {
    use std::io::Write;

    use super::{ImageId, write_param};

    #[derive(Default, PartialEq, Clone, Copy, Debug)]
    #[repr(u8)]
    pub enum CursorMovementPolicy {
        #[default]
        MoveToAfterImage = 0,
        DontMove = 1,
    }

    #[derive(Default, Debug, PartialEq, Clone)]
    pub struct DisplayConfig {
        pub location: DisplayLocation,
        pub cursor_movement: CursorMovementPolicy,
        pub create_virtual_placement: bool,
        pub parent_id: Option<ImageId>,
        pub parent_placement: Option<ImageId>,
    }

    impl DisplayConfig {
        pub(crate) fn write_to<W: Write>(&self, mut writer: W) -> std::io::Result<W> {
            writer = self.location.write_to(writer)?;
            if self.cursor_movement != CursorMovementPolicy::default() {
                write!(writer, ",C={}", self.cursor_movement as u8)?;
            }
            if self.create_virtual_placement {
                write!(writer, ",U=1")?;
            }
            if let Some(parent_id) = self.parent_id {
                writer = write_param(writer, 'P', parent_id.get(), 0)?;
            }
            if let Some(parent_placement) = self.parent_placement {
                writer = write_param(writer, 'Q', parent_placement.get(), 0)?;
            }
            Ok(writer)
        }
    }

    #[derive(Default, Debug, PartialEq, Clone)]
    pub struct DisplayLocation {
        pub x: u32,
        pub y: u32,
        pub width: u32,
        pub height: u32,
        pub x_offset: usize,
        pub y_offset: usize,
        pub columns: u16,
        pub rows: u16,
        pub z_index: i32,
        pub horizontal_offset: i32,
        pub vertical_offset: i32,
    }

    impl DisplayLocation {
        fn write_to<W: Write>(&self, writer: W) -> std::io::Result<W> {
            let writer = write_param(writer, 'x', self.x, 0)?;
            let writer = write_param(writer, 'y', self.y, 0)?;
            let writer = write_param(writer, 'w', self.width, 0)?;
            let writer = write_param(writer, 'h', self.height, 0)?;
            let writer = write_param(writer, 'X', self.x_offset, 0)?;
            let writer = write_param(writer, 'Y', self.y_offset, 0)?;
            let writer = write_param(writer, 'c', self.columns, 0)?;
            let writer = write_param(writer, 'r', self.rows, 0)?;
            let writer = write_param(writer, 'z', self.z_index, 0)?;
            let writer = write_param(writer, 'H', self.horizontal_offset, 0)?;
            write_param(writer, 'V', self.vertical_offset, 0)
        }
    }
}

pub mod delete {
    use std::{io::Write, ops::RangeInclusive};

    use super::{ImageId, PlacementId};

    #[derive(PartialEq, Debug, Clone)]
    pub struct DeleteConfig {
        pub effect: ClearOrDelete,
        pub which: WhichToDelete,
    }

    #[derive(PartialEq, Debug, Clone, Copy)]
    pub enum ClearOrDelete {
        Clear,
        Delete,
    }

    impl ClearOrDelete {
        fn maybe_upper(self, c: char) -> char {
            match self {
                Self::Delete => c.to_ascii_uppercase(),
                Self::Clear => c,
            }
        }
    }

    #[derive(PartialEq, Debug, Clone)]
    pub enum WhichToDelete {
        ImageId(ImageId, Option<PlacementId>),
        IdRange(RangeInclusive<ImageId>),
    }

    impl DeleteConfig {
        pub(crate) fn write_to<W: Write>(&self, mut writer: W) -> std::io::Result<W> {
            let effect = self.effect;
            write!(writer, ",d=")?;

            match &self.which {
                WhichToDelete::ImageId(image_id, placement_id) => {
                    write!(writer, "{},i={image_id}", effect.maybe_upper('i'))?;
                    if let Some(placement_id) = placement_id {
                        write!(writer, ",p={placement_id}")?;
                    }
                }
                WhichToDelete::IdRange(range) => write!(
                    writer,
                    "{},x={},y={}",
                    effect.maybe_upper('r'),
                    range.start(),
                    range.end()
                )?,
            }

            write!(writer, "\x1b\\")?;
            Ok(writer)
        }
    }
}

pub mod image {
    use std::{io::Write, num::NonZeroU32, str::Split, time::Duration};

    use super::{
        AnyValueOrSpecific, AsyncInputReader, IdentifierType, ImageDimensions, ImageId, NumberOrId,
        PixelFormat, PlacementId, Verbosity,
        display::DisplayConfig,
        error::{ParseError, TerminalError, TransmitError},
        medium::Medium,
        write_param,
    };

    #[derive(Debug, PartialEq)]
    pub struct Image<'data> {
        pub num_or_id: NumberOrId,
        pub format: PixelFormat,
        pub medium: Medium<'data>,
    }

    impl Image<'_> {
        pub(super) fn write_transmit_to<W: Write>(
            &self,
            mut writer: W,
            placement_id: Option<NonZeroU32>,
            display_config: Option<&DisplayConfig>,
            verbosity: Verbosity,
        ) -> std::io::Result<W> {
            match self.num_or_id {
                NumberOrId::Id(id) => write!(writer, "i={id}")?,
                NumberOrId::Number(num) => write!(writer, "I={num}")?,
            }

            if let Some(placement_id) = placement_id {
                writer = write_param(writer, 'p', placement_id.get(), 0)?;
            }

            writer = write_param(writer, 'q', verbosity as u8, 0)?;

            if let Some(config) = display_config {
                writer = config.write_to(writer)?;
            }

            writer = self.format.write_to(writer)?;
            writer = self.medium.write_data(writer)?;
            write!(writer, "\x1b\\")?;
            Ok(writer)
        }
    }

    impl From<::image::DynamicImage> for Image<'static> {
        fn from(value: ::image::DynamicImage) -> Self {
            let (format, data) = Image::fmt_and_data_from(value);

            Self {
                num_or_id: NumberOrId::Number(NonZeroU32::MIN),
                format,
                medium: Medium::Direct {
                    chunk_size: Some(super::medium::ChunkSize::default()),
                    data: data.into(),
                },
            }
        }
    }

    #[derive(Debug, thiserror::Error)]
    pub enum ImageFromShmFailureStep<E>
    where
        E: core::fmt::Display,
    {
        #[error("Couldn't create the shm: {0}")]
        ShmCreation(E),
        #[error("Couldn't copy over the provided image data into the shm: {0}")]
        DataCopy(std::io::Error),
    }

    impl Image<'_> {
        #[cfg(unix)]
        pub fn shm_from(
            image: ::image::DynamicImage,
            name: &str,
        ) -> Result<Self, ImageFromShmFailureStep<super::medium::ShmError>> {
            use super::medium::SharedMemObject;

            let (format, data) = Image::fmt_and_data_from(image);
            let mut obj = SharedMemObject::create_new(name, data.len())
                .map_err(ImageFromShmFailureStep::ShmCreation)?;
            obj.copy_in_buf(&data)
                .map_err(ImageFromShmFailureStep::DataCopy)?;

            Ok(Self {
                num_or_id: NumberOrId::Number(NonZeroU32::MIN),
                format,
                medium: Medium::SharedMemObject(obj),
            })
        }

        #[must_use]
        pub fn fmt_and_data_from(image: ::image::DynamicImage) -> (PixelFormat, Vec<u8>) {
            use ::image::DynamicImage::*;

            let dimensions = ImageDimensions {
                width: image.width(),
                height: image.height(),
            };

            match image {
                ImageLuma8(_) | ImageRgb8(_) | ImageLuma16(_) | ImageRgb16(_) | ImageRgb32F(_) => {
                    (PixelFormat::Rgb24(dimensions), image.into_rgb8().into_vec())
                }
                ImageLumaA8(_) | ImageRgba8(_) | ImageLumaA16(_) | ImageRgba16(_)
                | ImageRgba32F(_) | _ => (
                    PixelFormat::Rgba32(dimensions),
                    image.into_rgba8().into_vec(),
                ),
            }
        }
    }

    pub(crate) async fn read_parse_response_async<I: AsyncInputReader>(
        mut reader: I,
        image: NumberOrId,
        placement_id: Option<PlacementId>,
    ) -> Result<ImageId, TransmitError<I::Error>> {
        let mut output = String::with_capacity("\x1b_Gi=;OK\x1b\\".len() + 10);
        if let Err(e) = reader
            .read_esc_delimited_str_with_timeout(&mut output, Duration::from_millis(1000))
            .await
        {
            return Err(TransmitError::ReadingInput(e));
        }

        log::trace!("got terminal output {output:?}");

        parse_response(output, image, placement_id).map_or_else(
            |e| Err(TransmitError::ParsingResponse(e)),
            |res| res.map_err(TransmitError::Terminal),
        )
    }

    fn parse_response(
        output: String,
        image: NumberOrId,
        placement_id: Option<ImageId>,
    ) -> Result<Result<ImageId, TerminalError>, ParseError> {
        if !output.starts_with("_G") {
            return Err(ParseError::NoStartSequence(output));
        }

        let input = output.trim_start_matches("_G");
        let mut split_iter = input.split(';');
        let before_semicolon = split_iter.next().unwrap_or(input);
        let Some(after_semicolon) = split_iter.next() else {
            return Err(ParseError::NoFinalSemicolon);
        };

        let options = before_semicolon
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.split('='));

        let mut found_place_id = None;
        let mut found_image_num = None;
        let mut found_image_id = None;

        let image_num = match image {
            NumberOrId::Number(i) => Some(i),
            NumberOrId::Id(_) => None,
        };

        for mut opt in options {
            fn check_next_id<'input>(
                iter: &mut impl Iterator<Item = &'input str>,
                expected: Option<NonZeroU32>,
                ty: IdentifierType,
            ) -> Result<Option<NonZeroU32>, ParseError> {
                match (expected, iter.next()) {
                    (Some(expected), Some(found)) => match found.parse::<NonZeroU32>() {
                        Ok(i) if i == expected => Ok(Some(i)),
                        _ => Err(ParseError::DifferentIdInResponse {
                            ty,
                            found: found.to_string(),
                            expected: AnyValueOrSpecific::Specific(expected),
                        }),
                    },
                    (Some(_), None) => Err(ParseError::NoResponseId { ty }),
                    (None, Some(s)) => {
                        if ty == IdentifierType::ImageId {
                            s.parse::<NonZeroU32>().map(Some).map_err(|_e| {
                                ParseError::DifferentIdInResponse {
                                    ty,
                                    found: s.to_string(),
                                    expected: AnyValueOrSpecific::Any,
                                }
                            })
                        } else {
                            Err(ParseError::IdInResponseButNotInRequest {
                                ty,
                                value: s.to_string(),
                            })
                        }
                    }
                    (None, None) => Ok(None),
                }
            }

            match <Split<'_, _> as Iterator>::next(&mut opt).unwrap() {
                "i" => {
                    found_image_id = check_next_id(
                        &mut opt,
                        match image {
                            NumberOrId::Id(i) => Some(i),
                            NumberOrId::Number(_) => None,
                        },
                        IdentifierType::ImageId,
                    )?;
                }
                "p" => {
                    found_place_id =
                        check_next_id(&mut opt, placement_id, IdentifierType::PlacementId)?;
                }
                "I" => {
                    found_image_num =
                        check_next_id(&mut opt, image_num, IdentifierType::ImageNumber)?;
                }
                s => return Err(ParseError::UnknownResponseKey(s.to_owned())),
            }
        }

        let Some(found_image_id) = found_image_id else {
            let val = match image {
                NumberOrId::Id(i) => AnyValueOrSpecific::Specific(i),
                NumberOrId::Number(_) => AnyValueOrSpecific::Any,
            };
            return Err(ParseError::MissingId {
                ty: IdentifierType::ImageId,
                val,
            });
        };

        if let Some(place_id) = found_place_id.is_none().then_some(placement_id).flatten() {
            return Err(ParseError::MissingId {
                ty: IdentifierType::PlacementId,
                val: AnyValueOrSpecific::Specific(place_id),
            });
        }

        if let Some(image_num) = found_image_num.is_none().then_some(image_num).flatten() {
            return Err(ParseError::MissingId {
                ty: IdentifierType::ImageNumber,
                val: AnyValueOrSpecific::Specific(image_num),
            });
        }

        if after_semicolon == "OK" || after_semicolon == "OK\n" {
            return Ok(Ok(found_image_id));
        }

        let mut split = after_semicolon.split(':');
        let (Some(code), Some(reason)) = (split.next(), split.next()) else {
            return Err(ParseError::MalformedError(after_semicolon.to_string()));
        };

        TerminalError::try_from((code, reason)).map_or_else(
            |e| {
                Err(ParseError::UnknownErrorCode {
                    code: e.0.to_owned(),
                    reason: reason.to_owned(),
                })
            },
            |e| Ok(Err(e)),
        )
    }
}

pub mod action {
    use std::io::Write;

    use super::{
        AsyncInputReader, ImageId, NumberOrId, PlacementId, Verbosity, delete::DeleteConfig,
        display::DisplayConfig, error::TransmitError, image::Image,
        image::read_parse_response_async,
    };

    #[derive(Debug, PartialEq)]
    pub enum Action<'image, 'data> {
        Display {
            image_id: ImageId,
            placement_id: PlacementId,
            config: DisplayConfig,
        },
        TransmitAndDisplay {
            image: Image<'data>,
            config: DisplayConfig,
            placement_id: Option<PlacementId>,
        },
        Query(&'image Image<'data>),
        Delete(DeleteConfig),
    }

    impl Action<'_, '_> {
        fn write_transmit_to<W: Write>(
            &self,
            mut writer: W,
            verbosity: Verbosity,
        ) -> Result<W, std::io::Error> {
            write!(writer, "\x1b_Ga=")?;
            let mut writer = match self {
                Self::TransmitAndDisplay {
                    image,
                    config,
                    placement_id,
                } => {
                    write!(writer, "T,")?;
                    image.write_transmit_to(writer, *placement_id, Some(config), verbosity)?
                }
                Self::Query(image) => {
                    write!(writer, "q,")?;
                    image.write_transmit_to(writer, None, None, verbosity)?
                }
                Self::Display {
                    image_id,
                    placement_id,
                    config,
                } => {
                    write!(writer, "p,i={image_id},p={placement_id}")?;
                    writer = config.write_to(writer)?;
                    write!(writer, "\x1b\\")?;
                    writer
                }
                Self::Delete(delete) => {
                    write!(writer, "d")?;
                    delete.write_to(writer)?
                }
            };

            writer.flush()?;
            Ok(writer)
        }

        fn extract_num_or_id_and_placement(&self) -> Option<(NumberOrId, Option<PlacementId>)> {
            match self {
                Self::TransmitAndDisplay {
                    image,
                    placement_id,
                    ..
                } => Some((image.num_or_id, *placement_id)),
                Self::Query(img) => Some((img.num_or_id, None)),
                Self::Display {
                    image_id,
                    placement_id,
                    ..
                } => Some((NumberOrId::Id(*image_id), Some(*placement_id))),
                Self::Delete(_) => None,
            }
        }

        pub async fn execute_async<W: Write, I: AsyncInputReader>(
            self,
            writer: W,
            reader: I,
        ) -> Result<(W, Option<ImageId>), TransmitError<I::Error>> {
            let id_and_p = self.extract_num_or_id_and_placement();
            let writer = self
                .write_transmit_to(writer, Verbosity::All)
                .map_err(TransmitError::Writing)?;

            let image_id = if let Some((id_or_num, placement_id)) = id_and_p {
                Some(read_parse_response_async(reader, id_or_num, placement_id).await?)
            } else {
                None
            };

            Ok((writer, image_id))
        }
    }
}

pub mod tmux {
    use std::io::Write;

    pub struct TmuxWriter<W: Write> {
        inner: W,
        wrote_first: bool,
    }

    impl<W: Write> TmuxWriter<W> {
        pub fn new(inner: W) -> Self {
            Self {
                inner,
                wrote_first: false,
            }
        }
    }

    impl<W: Write> Write for TmuxWriter<W> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if !self.wrote_first {
                self.inner.write_all(b"\x1bPtmux;")?;
                self.wrote_first = true;
            }

            let mut last_esc = 0;
            for (idx, byte) in buf.iter().enumerate() {
                if *byte == 0x1b {
                    self.inner.write_all(&buf[last_esc..idx])?;
                    self.inner.write_all(&[0x1b])?;
                    last_esc = idx;
                }
            }
            self.inner.write_all(&buf[last_esc..])?;

            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.write_all(b"\x1b\\")?;
            self.wrote_first = false;
            self.inner.flush()
        }
    }
}

pub use action::Action;
pub use delete::{ClearOrDelete, DeleteConfig, WhichToDelete};
pub use display::{CursorMovementPolicy, DisplayConfig, DisplayLocation};
pub use error::{ParseError, TransmitError};
pub use event_stream::InputErr;
pub use image::Image;
pub use medium::Medium;
pub use tmux::TmuxWriter;

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

/// The original protocol helper uses a 1-second timeout for reading the terminal's response
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

/// Execute a single Kitty graphics action and await the terminal's response.
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

    let Ok(mut k_img) = Image::shm_from(img, shm_name) else {
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
                    format: PixelFormat::Rgb24(ImageDimensions {
                        width: 0,
                        height: 0,
                    }),
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
