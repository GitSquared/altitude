//! A quiet contour field for the space around Altitude's work.
//!
//! The field has its own clock. It runs at thirty frames a second, but only
//! samples the noise a few times a second: the frames in between interpolate
//! between two sampled height fields, so the contours move smoothly without
//! paying for a Perlin evaluation on every frame.

use std::{mem, time::Duration};

use ratatui::{
    buffer::{Buffer, Cell},
    layout::Rect,
};

use crate::tui::widgets::palette;

/// How often the contour field advances.
///
/// Smoothness is most of what a backdrop is for. The sampling below is what keeps
/// thirty frames a second from costing thirty noise evaluations.
pub const CLOCK: Duration = Duration::from_millis(33);

/// How many frames sit between two sampled height fields.
const KEYFRAME_TICKS: usize = 24;

/// How fast the terrain evolves along the noise field's third axis.
///
/// The third axis is time: the land changes shape in place instead of sliding.
const MORPH_PER_SECOND: f64 = 0.01;

/// The noise-space extent of the dominant dimension. The other dimension
/// follows from the window's shape, so the texture is never stretched.
const WORLD_WIDTH: f64 = 3.2;
const WORLD_HEIGHT: f64 = 2.0;

/// How many octaves of terrain, and how many contour lines per unit of height.
const OCTAVES: usize = 3;
const LEVELS: f64 = 14.3;

/// The shape of the fractal: wide features, with the octaves close together and
/// the higher ones a little stronger.
const FREQUENCY: f64 = 0.7;
const GAIN: f64 = 0.62;
const LACUNARITY: f64 = 1.55;

/// How far the base elevation pushes the next noise sample along its third axis.
///
/// The elevation feeds back into the noise coordinate instead of only advancing
/// with time, which is what gives the field its coherent relief.
const ELEVATION_AXIS: f64 = 0.28;

/// Where the field starts along the noise's third axis. Not load-bearing: any
/// slice is a valid terrain, this one just happens to be a known-good one.
const Z_ORIGIN: f64 = 15.246592;

/// Push middle elevations down into valleys, leaving the peaks as the detail.
const REDISTRIBUTION: f64 = 2.0;

/// How much of the broad landscape shape to mix back in.
///
/// A little goes a long way: it gives the contours a horizon and makes the field
/// read as relief rather than as an even field of noise.
const SWELL_WEIGHT: f64 = 0.16;

/// What a cell is marked with before the interface draws over it.
///
/// A private-use scalar no widget writes. Whatever still carries it after the
/// screen has drawn is a cell the field is allowed to show through, which is what
/// lets every widget stay unaware of the backdrop: an opaque widget writes its
/// own cells, a translucent one leaves them alone.
const OPEN_SYMBOL: &str = "\u{e000}";
const OPEN_CELL: Cell = Cell::new(OPEN_SYMBOL);

/// The braille dot each position in a cell's 2×4 pixel grid stands for.
const DOTS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

#[derive(Default)]
pub struct Terrain {
    tick: usize,
    /// The two sampled height fields the current frame interpolates between.
    heights: [Vec<f64>; 2],
    /// The interpolated heights the glyphs are packed from.
    current: Vec<f64>,
    /// Pixel corners and rows in noise space, reused between frames.
    xs: Vec<f64>,
    ys: Vec<f64>,
    /// What the last pack found for every cell in the field's area.
    glyphs: Vec<Cell>,
    /// The cells the last pack was for, so a frame that changes neither replays.
    open: Vec<(u16, u16)>,
    /// Where this frame found open cells, kept to reuse its allocation.
    scan: Vec<(u16, u16)>,
    /// Open cell runs per cell row, for evaluating only the visible heights.
    runs: Vec<Vec<(u16, u16)>>,
    /// The tick and area the glyphs were packed for.
    drawn: Option<(usize, Rect)>,
    /// The keyframe index and area the sampled heights are for.
    sampled: Option<(usize, Rect)>,
}

impl Terrain {
    pub fn advance(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// Mark the field's area before the interface draws over it.
    pub fn mark(&self, buffer: &mut Buffer, area: Rect) {
        for y in area.top()..area.bottom() {
            let start = buffer.index_of(area.left(), y);
            buffer.content[start..start + usize::from(area.width)].fill(OPEN_CELL.clone());
        }
    }

    /// Draw the field into whatever the interface left open.
    pub fn draw(&mut self, buffer: &mut Buffer, area: Rect) {
        let mut scan = mem::take(&mut self.scan);
        scan.clear();
        collect_open(buffer, area, &mut scan);
        if area.width < 8 || area.height < 4 {
            for &(x, y) in &scan {
                buffer[(x, y)] = Cell::EMPTY;
            }
            self.open.clear();
            self.open.extend_from_slice(&scan);
            self.scan = scan;
            return;
        }

        let same = self.drawn == Some((self.tick, area)) && scan == self.open;
        if !same {
            let moved = self.open != scan;
            self.update(area, &scan, moved);
            self.open.clear();
            self.open.extend_from_slice(&scan);
            self.drawn = Some((self.tick, area));
        }

        self.paint(buffer, area);
        self.scan = scan;
    }

    /// Repaint the cells the last draw left open, without looking for them again.
    ///
    /// For a frame where only the field moved, the interface above it is the one
    /// the last frame already had, so the open cells are still the open cells.
    pub fn repaint(&mut self, buffer: &mut Buffer) {
        let Some((_, area)) = self.drawn else {
            return;
        };
        if self.drawn != Some((self.tick, area)) {
            let open = mem::take(&mut self.open);
            self.update(area, &open, false);
            self.open = open;
            self.drawn = Some((self.tick, area));
        }
        self.paint(buffer, area);
    }

    fn paint(&self, buffer: &mut Buffer, area: Rect) {
        let width = usize::from(area.width);
        for &(x, y) in &self.open {
            let index = usize::from(y - area.y) * width + usize::from(x - area.x);
            buffer[(x, y)] = self.glyphs[index].clone();
        }
    }

    fn update(&mut self, area: Rect, open: &[(u16, u16)], moved: bool) {
        let height = usize::from(area.height);
        let pixel_width = usize::from(area.width) * 2;
        let pixel_height = height * 4;
        let stride = pixel_width + 1;
        let keyframe = self.tick / KEYFRAME_TICKS;
        let phase = (self.tick % KEYFRAME_TICKS) as f64 / KEYFRAME_TICKS as f64;

        if moved || self.sampled.is_none_or(|(_, sampled)| sampled != area) {
            self.layout(area, open, pixel_width, pixel_height);
        }
        if moved
            || self
                .sampled
                .is_none_or(|(sampled, sampled_area)| sampled != keyframe || sampled_area != area)
        {
            self.sample(area, keyframe, moved, stride, pixel_height);
        }
        self.interpolate(pixel_height, stride, phase);
        self.pack(area, stride);
    }

    fn layout(&mut self, area: Rect, open: &[(u16, u16)], pixel_width: usize, pixel_height: usize) {
        // Fit the same world to the current window, with one noise scale in both
        // directions. Resizing zooms the terrain instead of revealing more of it,
        // so a screen of any size carries the same amount of terrain and the same
        // contour density.
        let scale = if pixel_width >= pixel_height {
            WORLD_WIDTH / pixel_width as f64
        } else {
            WORLD_HEIGHT / pixel_height as f64
        };
        let half_width = pixel_width as f64 / 2.0;
        let half_height = pixel_height as f64 / 2.0;
        self.xs.resize(pixel_width + 1, 0.0);
        self.ys.resize(pixel_height + 1, 0.0);
        for (column, x) in self.xs.iter_mut().enumerate() {
            *x = (column as f64 - half_width) * scale;
        }
        for (row, y) in self.ys.iter_mut().enumerate() {
            *y = (row as f64 - half_height) * scale;
        }
        let height = usize::from(area.height);
        if self.runs.len() < height {
            self.runs.resize_with(height, Vec::new);
        }
        for runs in &mut self.runs[..height] {
            runs.clear();
        }
        for &(x, y) in open {
            let row = usize::from(y - area.y);
            let column = x - area.x;
            let runs = &mut self.runs[row];
            match runs.last_mut() {
                Some(last) if last.1 == column => last.1 = column + 1,
                _ => runs.push((column, column + 1)),
            }
        }
    }

    /// Sample the height fields that bracket the current keyframe.
    fn sample(
        &mut self,
        area: Rect,
        keyframe: usize,
        moved: bool,
        stride: usize,
        pixel_height: usize,
    ) {
        // One keyframe on, the second field is already the first field of the new
        // pair. Anything else — the first draw, a resize, or a jump — needs both.
        let can_shift = !moved
            && self.sampled.is_some_and(|(sampled, sampled_area)| {
                sampled_area == area && sampled + 1 == keyframe
            });
        if can_shift {
            self.heights.swap(0, 1);
            self.sample_into(area, keyframe + 1, 1, stride, pixel_height);
        } else {
            self.sample_into(area, keyframe, 0, stride, pixel_height);
            self.sample_into(area, keyframe + 1, 1, stride, pixel_height);
        }
        self.sampled = Some((keyframe, area));
    }

    fn sample_into(
        &mut self,
        area: Rect,
        keyframe: usize,
        slot: usize,
        stride: usize,
        pixel_height: usize,
    ) {
        let z = Z_ORIGIN
            + keyframe as f64 * KEYFRAME_TICKS as f64 * CLOCK.as_secs_f64() * MORPH_PER_SECOND;
        let height = usize::from(area.height);
        let heights = &mut self.heights[slot];
        heights.clear();
        heights.resize(stride * (pixel_height + 1), 0.0);
        let field = Field { xs: &self.xs, z };
        let runs = &self.runs[..height];
        for pixel_row in 0..=pixel_height {
            let row = (pixel_row / 4).min(height - 1);
            let y = self.ys[pixel_row];
            fill_run_heights(heights, pixel_row * stride, &runs[row], y, &field);
            // This pixel row is the bottom of one row of cells and the top of the
            // next, so it has to cover both rows' open columns. When both rows are
            // open in the same places, the second pass only repeats.
            if pixel_row % 4 == 0 && row > 0 && runs[row] != runs[row - 1] {
                fill_run_heights(heights, pixel_row * stride, &runs[row - 1], y, &field);
            }
        }
    }

    fn interpolate(&mut self, pixel_height: usize, stride: usize, phase: f64) {
        let len = stride * (pixel_height + 1);
        self.current.resize(len, 0.0);
        let first = &self.heights[0];
        let second = &self.heights[1];
        if phase == 0.0 {
            self.current.copy_from_slice(first);
            return;
        }
        for (current, (first, second)) in
            self.current.iter_mut().zip(first.iter().zip(second.iter()))
        {
            *current = first + (second - first) * phase;
        }
    }

    fn pack(&mut self, area: Rect, stride: usize) {
        let height = usize::from(area.height);
        let glyphs = &mut self.glyphs;
        glyphs.clear();
        glyphs.resize(usize::from(area.width) * height, Cell::EMPTY);
        let heights = &self.current;
        for row in 0..height {
            for &(x0, x1) in &self.runs[row] {
                for column in x0..x1 {
                    let mut dots = 0;
                    for (dot_row, row_dots) in DOTS.iter().enumerate() {
                        for (dot_column, dot) in row_dots.iter().enumerate() {
                            let index =
                                (row * 4 + dot_row) * stride + usize::from(column) * 2 + dot_column;
                            let a = heights[index];
                            let b = heights[index + 1];
                            let c = heights[index + stride];
                            let d = heights[index + stride + 1];
                            let minimum = a.min(b).min(c.min(d));
                            let maximum = a.max(b).max(c.max(d));
                            let level = (minimum * LEVELS).floor().max(0.0) + 1.0;
                            if level <= maximum * LEVELS {
                                dots |= *dot;
                            }
                        }
                    }
                    if dots != 0 {
                        let symbol = char::from_u32(0x2800 + u32::from(dots))
                            .expect("braille dots always form a Unicode scalar");
                        let mut cell = Cell::EMPTY;
                        cell.set_char(symbol).set_style(palette::backdrop());
                        glyphs[row * usize::from(area.width) + usize::from(column)] = cell;
                    }
                }
            }
        }
    }
}

fn collect_open(buffer: &Buffer, area: Rect, open: &mut Vec<(u16, u16)>) {
    for y in area.top()..area.bottom() {
        let start = buffer.index_of(area.left(), y);
        let row = &buffer.content[start..start + usize::from(area.width)];
        for (offset, cell) in row.iter().enumerate() {
            if cell.symbol() == OPEN_SYMBOL {
                open.push((area.left() + offset as u16, y));
            }
        }
    }
}

struct Field<'a> {
    xs: &'a [f64],
    z: f64,
}

fn fill_run_heights(heights: &mut [f64], base: usize, runs: &[(u16, u16)], y: f64, field: &Field) {
    for &(x0, x1) in runs {
        let first = usize::from(x0) * 2;
        let last = usize::from(x1) * 2;
        for column in first..=last {
            heights[base + column] = elevation(field.xs[column], y, field.z);
        }
    }
}

#[inline(always)]
fn elevation(x: f64, y: f64, z: f64) -> f64 {
    let mut height = texture(x, y, z);
    if ELEVATION_AXIS > 0.0 {
        let shifted = z + (height - 0.5) * ELEVATION_AXIS * 6.0;
        height = texture(x, y, shifted);
    }
    let swell = 1.30 - ((x * 0.38).powi(2) + (y * 0.80).powi(2)).sqrt();
    height.powf(REDISTRIBUTION) + SWELL_WEIGHT * swell
}

/// The normalized fractal noise, before redistribution and the broad swell.
#[inline(always)]
fn texture(x: f64, y: f64, z: f64) -> f64 {
    let mut texture = 0.0;
    let mut frequency = FREQUENCY;
    let mut amplitude = 1.0;
    let mut total = 0.0;
    for _ in 0..OCTAVES {
        texture += perlin3(x * frequency, y * frequency, z * frequency) * amplitude;
        total += amplitude;
        frequency *= LACUNARITY;
        amplitude *= GAIN;
    }
    (0.5 + 0.5 * (texture / total)).clamp(0.0, 1.0)
}

/// Three-dimensional gradient noise, sliced at `z`.
///
/// The third axis is time, so the land changes in place rather than sliding
/// under a moving window.
#[inline(always)]
fn perlin3(x: f64, y: f64, z: f64) -> f64 {
    const GRADIENTS: [(f64, f64, f64); 16] = [
        (1.0, 1.0, 0.0),
        (-1.0, 1.0, 0.0),
        (1.0, -1.0, 0.0),
        (-1.0, -1.0, 0.0),
        (1.0, 0.0, 1.0),
        (-1.0, 0.0, 1.0),
        (1.0, 0.0, -1.0),
        (-1.0, 0.0, -1.0),
        (0.0, 1.0, 1.0),
        (0.0, -1.0, 1.0),
        (0.0, 1.0, -1.0),
        (0.0, -1.0, -1.0),
        (1.0, 1.0, 0.0),
        (-1.0, 1.0, 0.0),
        (1.0, -1.0, 0.0),
        (-1.0, -1.0, 0.0),
    ];
    #[inline(always)]
    fn fade(t: f64) -> f64 {
        t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
    }
    #[inline(always)]
    fn dot(corner_x: i64, corner_y: i64, corner_z: i64, dx: f64, dy: f64, dz: f64) -> f64 {
        let hash = (corner_x.wrapping_mul(374_761_393)
            ^ corner_y.wrapping_mul(668_265_263)
            ^ corner_z.wrapping_mul(1_440_662_683))
        .wrapping_mul(1_274_126_177);
        let gradient = GRADIENTS[(hash & 15) as usize];
        gradient.0 * dx + gradient.1 * dy + gradient.2 * dz
    }
    let left = x.floor() as i64;
    let top = y.floor() as i64;
    let back = z.floor() as i64;
    let u = x - left as f64;
    let v = y - top as f64;
    let w = z - back as f64;
    let n000 = dot(left, top, back, u, v, w);
    let n100 = dot(left + 1, top, back, u - 1.0, v, w);
    let n010 = dot(left, top + 1, back, u, v - 1.0, w);
    let n110 = dot(left + 1, top + 1, back, u - 1.0, v - 1.0, w);
    let n001 = dot(left, top, back + 1, u, v, w - 1.0);
    let n101 = dot(left + 1, top, back + 1, u - 1.0, v, w - 1.0);
    let n011 = dot(left, top + 1, back + 1, u, v - 1.0, w - 1.0);
    let n111 = dot(left + 1, top + 1, back + 1, u - 1.0, v - 1.0, w - 1.0);
    let fu = fade(u);
    let fv = fade(v);
    let fw = fade(w);
    let x00 = n000 + fu * (n100 - n000);
    let x10 = n010 + fu * (n110 - n010);
    let x01 = n001 + fu * (n101 - n001);
    let x11 = n011 + fu * (n111 - n011);
    let y0 = x00 + fv * (x10 - x00);
    let y1 = x01 + fv * (x11 - x01);
    y0 + fw * (y1 - y0)
}
