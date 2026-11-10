mod kitty;
mod mermaid;

use std::{
    collections::HashMap,
    io::Cursor,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
    time::Duration,
};

use image::{DynamicImage, ImageFormat};
use ratatui::{
    Frame,
    layout::{Rect, Size},
};
use ratatui_image::{
    picker::{Capability, Picker, ProtocolType, cap_parser::QueryStdioOptions},
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};

use self::kitty::{Cells, Placement};
use crate::task::picture;

// Long enough for a terminal across an ssh link to answer, short enough that one
// that never answers does not hold the interface up.
const ANSWER_TIMEOUT: Duration = Duration::from_millis(800);
// Rasters are kept so a second reading of the same diagram costs nothing; only the
// ones a conversation is showing stay warm.
const WARM_RASTERS: usize = 12;

pub struct Pictures {
    picker: Picker,
    is_dark: bool,
    // A diagram is laid out, drawn and encoded off this thread: the interface is
    // waiting for the answer, and nothing about drawing one needs to happen in front
    // of the operator. The cell size it measures with, and where the pictures a signal
    // carries are kept, go with it.
    drawing: Sender<Job>,
    finished: Receiver<Answer>,
    // What has been asked for and not answered yet, by the digest that names it and
    // the column it was asked for in.
    asked: HashMap<u64, u16>,
    rasters: HashMap<u64, Option<Raster>>,
    // Only the protocols the crate lays out itself use this: a kitty picture is placed
    // by cells, so nothing about it needs a protocol object.
    sliced: HashMap<SlicedKey, Option<Sliced>>,
    cells: Cells,
}

// A diagram or an attached picture, drawn once and kept: what the terminal is given
// of it, the cells it covers at its own scale, which is where every other size is
// measured from, and the column it was drawn for, which is as wide as it is worth.
struct Raster {
    natural: Size,
    columns: u16,
    pixels: Pixels,
}

// What is kept of a picture: a terminal that decodes the PNG it is sent never reads
// the pixels, so only the encoding is held on to; one the crate slices an image into
// cells is given them instead, because that is the only form it can place.
enum Pixels {
    Encoded { data: Vec<u8>, height: u32 },
    Raw(DynamicImage),
}

// A picture the drawing thread has made and not yet encoded: the pixels, the cells
// they cover at their own scale, and the column they were drawn for.
pub(super) struct Drawn {
    pub(super) picture: DynamicImage,
    pub(super) natural: Size,
    pub(super) columns: u16,
}

struct Job {
    digest: u64,
    source: String,
    columns: u16,
}

struct Answer {
    digest: u64,
    columns: u16,
    raster: Option<Raster>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct SlicedKey {
    source: u64,
    columns: u16,
}

struct Sliced {
    protocol: SlicedProtocol,
    size: Size,
}

impl Pictures {
    pub fn detect(attachments: PathBuf) -> Self {
        let options = QueryStdioOptions {
            timeout: ANSWER_TIMEOUT,
            terminal_background_color_osc: true,
            ..QueryStdioOptions::default()
        };
        let picker =
            Picker::from_query_stdio_with_options(options).unwrap_or_else(|_| Picker::halfblocks());
        // A terminal that does not say which background it has is assumed dark,
        // which is the theme most of them are set to.
        let is_dark = picker
            .capabilities()
            .iter()
            .find_map(|capability| match capability {
                Capability::Background(red, green, blue) => {
                    Some(is_dark_colour(*red, *green, *blue))
                }
                _ => None,
            })
            .unwrap_or(true);
        // The size a terminal reports for a cell is the size it lays pictures out in,
        // asked once here and carried to the thread that draws them.
        let font = picker.font_size();
        let cell = Size::new(font.width.max(1), font.height.max(1));
        let (drawing, jobs) = mpsc::channel();
        let (answers, finished) = mpsc::channel();
        let drawn_attachments = attachments.clone();
        // The one protocol that places a picture by naming its cells is also the one
        // that decodes the PNG it is sent, so its pictures are kept encoded and no
        // others are.
        let encoded = picker.protocol_type() == ProtocolType::Kitty;
        // Named so a thread that is wedged can be found, and detached so a diagram
        // still being drawn never holds the interface open on its way out.
        let _ = thread::Builder::new()
            .name("pictures".to_owned())
            .spawn(move || {
                draw_off_thread(jobs, answers, drawn_attachments, is_dark, cell, encoded)
            });
        Self {
            picker,
            is_dark,
            drawing,
            finished,
            asked: HashMap::new(),
            rasters: HashMap::new(),
            sliced: HashMap::new(),
            cells: Cells::default(),
        }
    }

    pub fn describe(&self) -> String {
        let font = self.picker.font_size();
        format!(
            "{:?} · font {}x{} · {}",
            self.picker.protocol_type(),
            font.width,
            font.height,
            if self.is_dark { "dark" } else { "light" }
        )
    }

    // Halfblocks are coloured squares: a diagram is mostly its labels, so the source
    // the operator can read beats a picture of it they cannot.
    pub fn has_pictures(&self) -> bool {
        self.picker.protocol_type() != ProtocolType::Halfblocks
    }

    // A picture is drawn as wide as the column it has, or as wide as it is when that
    // is narrower, and as tall as its own proportions then make it: a diagram reads at
    // the size its words are set to, and the column decides how much of that it keeps.
    // One still being drawn off the loop is nothing to draw yet, so its source stays.
    pub fn size(&mut self, source: &str, columns: u16) -> Option<Size> {
        let natural = self.warm(source, columns)?.natural;
        let box_size = box_of(natural, drawn_scale(natural, columns));
        // A picture placed by cells covers exactly the cells it names, so the box is
        // the size; the crate reports what it will draw for everything else.
        if self.is_placing_by_cells() {
            return Some(box_size);
        }
        self.slice(source, columns).map(|sliced| sliced.size)
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        viewport: Rect,
        source: &str,
        columns: u16,
        at: (i16, i16),
    ) {
        let Some(natural) = self.warm(source, columns).map(|raster| raster.natural) else {
            return;
        };
        let box_size = box_of(natural, drawn_scale(natural, columns));
        if self.is_placing_by_cells() {
            let digest = digest(source);
            let Some(raster) = self.rasters.get(&digest).and_then(Option::as_ref) else {
                return;
            };
            let Pixels::Encoded { data, height } = &raster.pixels else {
                return;
            };
            let _ = frame;
            self.cells.plan(
                viewport,
                Placement {
                    key: digest,
                    png: data,
                    pixels_high: *height,
                    columns: box_size.width,
                    rows: box_size.height,
                    at,
                },
            );
            return;
        }
        // Every other protocol is laid out by the crate, which knows how to slice a
        // sixel and how much of an iTerm2 image a terminal will take.
        if let Some(sliced) = self.slice(source, columns) {
            let at = SignedPosition::from(at);
            frame.render_widget(SlicedImage::new(&sliced.protocol, at), viewport);
        }
    }

    // Whether a picture has been drawn since the last frame was: it is taken in here,
    // and the frame that does it is the frame that shows it. One still on its way is
    // not a reason to draw anything, because there is nothing yet to show.
    pub fn arrived(&mut self) -> bool {
        let mut arrived = false;
        loop {
            match self.finished.try_recv() {
                Ok(answer) => {
                    if self.asked.get(&answer.digest) != Some(&answer.columns) {
                        continue;
                    }
                    self.asked.remove(&answer.digest);
                    if self.rasters.len() >= WARM_RASTERS {
                        self.rasters.clear();
                        self.sliced.clear();
                    }
                    self.rasters.insert(answer.digest, answer.raster);
                    arrived = true;
                }
                Err(TryRecvError::Empty) => break,
                // Nothing is drawing and nothing will, so what was asked for is
                // forgotten rather than waited on.
                Err(TryRecvError::Disconnected) => {
                    self.asked.clear();
                    break;
                }
            }
        }
        arrived
    }

    // What the terminal has to be told after this frame: pictures are positioned by
    // the cursor, so they are written once the frame is on screen.
    pub fn settle(&mut self) -> String {
        self.cells.settle()
    }

    fn is_placing_by_cells(&self) -> bool {
        self.picker.protocol_type() == ProtocolType::Kitty
    }

    fn warm(&mut self, source: &str, columns: u16) -> Option<&Raster> {
        let digest = digest(source);
        let drawn_for = self
            .rasters
            .get(&digest)
            .and_then(Option::as_ref)
            .map_or(0, |raster| raster.columns);
        // A picture drawn wider than the pixels it was made for is asked for again: a
        // window that grew would otherwise show it soft for as long as it is open.
        if drawn_for < columns && self.asked.get(&digest) != Some(&columns) {
            // The newest column wins: a picture being dragged wider is asked for once
            // per frame, and only the width it ends at is worth drawing. A thread that
            // is not there to take it leaves the source where it is rather than a
            // question nothing will answer.
            let job = Job {
                digest,
                source: source.to_owned(),
                columns,
            };
            if self.drawing.send(job).is_ok() {
                self.asked.insert(digest, columns);
            }
        }
        self.rasters.get(&digest)?.as_ref()
    }

    fn slice(&mut self, source: &str, columns: u16) -> Option<&Sliced> {
        let key = SlicedKey {
            source: digest(source),
            columns,
        };
        if !self.sliced.contains_key(&key) {
            let sliced = build_sliced(
                &self.picker,
                self.rasters.get(&key.source)?.as_ref()?,
                columns,
            );
            self.sliced.insert(key, sliced);
        }
        self.sliced.get(&key)?.as_ref()
    }
}

fn build_sliced(picker: &Picker, raster: &Raster, columns: u16) -> Option<Sliced> {
    let Pixels::Raw(picture) = &raster.pixels else {
        return None;
    };
    let box_size = box_of(raster.natural, drawn_scale(raster.natural, columns));
    let protocol = SlicedProtocol::new(picker, picture.clone(), Some(box_size)).ok()?;
    let size = protocol.size();
    Some(Sliced { protocol, size })
}

// The pictures, drawn as they are asked for and answered on their way back. One
// thread, so a diagram is laid out and rasterized in the order the conversation asked
// for them, and nothing it does can stall an interface.
fn draw_off_thread(
    jobs: Receiver<Job>,
    answers: Sender<Answer>,
    attachments: PathBuf,
    is_dark: bool,
    cell: Size,
    encoded: bool,
) {
    while let Ok(job) = jobs.recv() {
        // Everything already queued is superseded by the newest column asked for: a
        // window being dragged asks for the same diagram at every width it passes, and
        // only where it ends up is worth drawing. The one left is the last one asked
        // for, which is the one the interface is waiting on.
        let mut batch = HashMap::from([(job.digest, job)]);
        while let Ok(job) = jobs.try_recv() {
            batch.insert(job.digest, job);
        }
        for job in batch.into_values() {
            let raster = build(
                &job.source,
                &attachments,
                is_dark,
                cell,
                job.columns,
                encoded,
            );
            let answer = Answer {
                digest: job.digest,
                columns: job.columns,
                raster,
            };
            if answers.send(answer).is_err() {
                return;
            }
        }
    }
}

fn build(
    source: &str,
    attachments: &Path,
    is_dark: bool,
    cell: Size,
    columns: u16,
    encoded: bool,
) -> Option<Raster> {
    let Drawn {
        picture,
        natural,
        columns,
    } = draw(source, attachments, is_dark, cell, columns)?;
    let pixels = match encoded {
        true => Pixels::Encoded {
            data: encode(&picture)?,
            height: picture.height(),
        },
        false => Pixels::Raw(picture),
    };
    Some(Raster {
        natural,
        columns,
        pixels,
    })
}

// The pixels as a PNG, which is what a terminal is sent: encoding them is the drawing
// thread's work, so the frame that shows a picture never pays for it.
fn encode(picture: &DynamicImage) -> Option<Vec<u8>> {
    let mut png = Vec::new();
    picture
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    Some(png)
}

fn draw(
    source: &str,
    attachments: &Path,
    is_dark: bool,
    cell: Size,
    columns: u16,
) -> Option<Drawn> {
    let Some(id) = source.strip_prefix(picture::PREFIX) else {
        return mermaid::raster(source, is_dark, cell, columns);
    };
    // A picture the host has already read out of a sandbox: its digest names the
    // bytes, and it is kept at the size it arrived at rather than redrawn.
    let picture = image::load_from_memory(&picture::read(attachments, id).ok()?).ok()?;
    let natural = arrived_cells(&picture, cell);
    // An arrived picture is not drawn at any size, so nothing about it is worth
    // asking for again.
    Some(Drawn {
        picture,
        natural,
        columns: u16::MAX,
    })
}

// An arrived picture is drawn at half its pixels, which is how a photograph of a screen
// full of text stays a picture of the card rather than of the terminal.
fn arrived_cells(picture: &DynamicImage, cell: Size) -> Size {
    let across = |pixels: u32, cell: u16| {
        u16::try_from(pixels.div_ceil(u32::from(cell.max(1)) * 2)).unwrap_or(u16::MAX)
    };
    Size::new(
        across(picture.width(), cell.width),
        across(picture.height(), cell.height),
    )
}

// At its own scale a picture's labels are one cell row tall, so the only question a
// column asks is how much of that scale it can hold.
fn drawn_scale(natural: Size, columns: u16) -> f32 {
    (f32::from(columns.max(1)) / f32::from(natural.width.max(1))).min(1.0)
}

// The cells a picture covers at that fraction, with the height following from the
// picture's own proportions rather than from a second rounding: a diagram is as tall
// as its content is, and a long one is read by scrolling.
fn box_of(natural: Size, scale: f32) -> Size {
    let wide = f32::from(natural.width.max(1));
    let tall = f32::from(natural.height.max(1));
    let width = (wide * scale.max(f32::MIN_POSITIVE)).round().max(1.0) as u16;
    Size::new(
        width,
        ((f32::from(width) * tall / wide).round().max(1.0) as u16).max(1),
    )
}

fn is_dark_colour(red: u8, green: u8, blue: u8) -> bool {
    let luma = 0.299 * f32::from(red) + 0.587 * f32::from(green) + 0.114 * f32::from(blue);
    luma < 128.0
}

fn digest(source: &str) -> u64 {
    let hash = blake3::hash(source.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(bytes)
}
