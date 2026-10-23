//! A quiet contour field for the space around Altitude's work.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
};

pub fn draw(frame: &mut Frame, area: Rect, tick: usize) {
    if area.width < 8 || area.height < 4 {
        return;
    }

    // Braille gives each terminal cell a 2×4 pixel grid, as in milli.nvim's
    // exported splashes. Sample the terrain once per pixel corner, then pack
    // contour crossings into the eight dots of each cell.
    let pixel_width = usize::from(area.width) * 2;
    let pixel_height = usize::from(area.height) * 4;
    let drift = tick as f32 * 0.0042;
    let stride = pixel_width + 1;
    let mut heights = vec![0.0; stride * (pixel_height + 1)];
    for row in 0..=pixel_height {
        for column in 0..=pixel_width {
            let x = column as f32 / pixel_width as f32 * 3.2;
            let y = row as f32 / pixel_height as f32 * 2.0;
            heights[row * stride + column] = elevation(x, y, drift);
        }
    }

    const DOTS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
    let buffer = frame.buffer_mut();
    for row in 0..area.height {
        for column in 0..area.width {
            let mut dots = 0;
            for (dot_row, row_dots) in DOTS.iter().enumerate() {
                for (dot_column, dot) in row_dots.iter().enumerate() {
                    let pixel_x = usize::from(column) * 2 + dot_column;
                    let pixel_y = usize::from(row) * 4 + dot_row;
                    let index = pixel_y * stride + pixel_x;
                    let corners = [
                        heights[index],
                        heights[index + 1],
                        heights[index + stride],
                        heights[index + stride + 1],
                    ];
                    let minimum = corners.into_iter().fold(f32::INFINITY, f32::min);
                    let maximum = corners.into_iter().fold(f32::NEG_INFINITY, f32::max);
                    let level = (minimum * 8.0).floor().max(0.0) + 1.0;
                    if level <= maximum * 8.0 {
                        dots |= *dot;
                    }
                }
            }
            if dots == 0 {
                continue;
            }
            let symbol = char::from_u32(0x2800 + u32::from(dots))
                .expect("braille dots always form a Unicode scalar")
                .to_string();
            // The theme's own foreground, dimmed rather than painted in greys of
            // our choosing: a contour field should sit a shade off the background
            // whichever background the operator has, and only the terminal knows
            // what that is.
            let style = Style::new().fg(Color::Reset).add_modifier(Modifier::DIM);
            buffer[(area.x + column, area.y + row)]
                .set_symbol(&symbol)
                .set_style(style);
        }
    }
}

fn elevation(x: f32, y: f32, drift: f32) -> f32 {
    // One broad swell, reaching past every edge, so the contours are a landscape
    // that fills the area rather than an island in the middle of it. The texture
    // is what makes it terrain; the swell is what gives it a horizon.
    let dx = (x - 1.6) * 0.38;
    let dy = (y - 1.0) * 0.60;
    let swell = 1.30 - (dx * dx + dy * dy).sqrt();
    let mut texture = 0.0;
    let mut frequency = 1.0;
    // Three octaves, not four: at this size the finest one only added jitter to
    // the contour lines, which reads as jaggedness rather than as terrain.
    let mut amplitude = 0.16;
    for _ in 0..3 {
        texture += perlin(x * frequency + drift, y * frequency + drift * 0.4) * amplitude;
        frequency *= 2.0;
        amplitude *= 0.5;
    }
    swell + texture
}

fn perlin(x: f32, y: f32) -> f32 {
    let left = x.floor() as i32;
    let top = y.floor() as i32;
    let u = x - left as f32;
    let v = y - top as f32;
    let fade = |t: f32| t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
    let dot = |corner_x: i32, corner_y: i32, offset_x: f32, offset_y: f32| {
        let hash = (corner_x.wrapping_mul(374_761_393) ^ corner_y.wrapping_mul(668_265_263))
            .wrapping_mul(1_274_126_177);
        match hash & 7 {
            0 => offset_x + offset_y,
            1 => offset_x - offset_y,
            2 => -offset_x + offset_y,
            3 => -offset_x - offset_y,
            4 => offset_x,
            5 => -offset_x,
            6 => offset_y,
            _ => -offset_y,
        }
    };
    let upper =
        dot(left, top, u, v) + fade(u) * (dot(left + 1, top, u - 1.0, v) - dot(left, top, u, v));
    let lower = dot(left, top + 1, u, v - 1.0)
        + fade(u) * (dot(left + 1, top + 1, u - 1.0, v - 1.0) - dot(left, top + 1, u, v - 1.0));
    upper + fade(v) * (lower - upper)
}
