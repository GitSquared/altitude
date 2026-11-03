use crossterm::terminal;
use std::{
    io::{self, Read, Write, stdout},
    sync::mpsc,
    thread,
    time::Duration,
};

// What one cell actually is, in pixels. A font size is not a cell: the line height a
// terminal is configured with makes cells taller than the type inside them, and every
// picture is laid out in cells. Asked here rather than assumed, because the answer
// decides how many rows a picture needs.
pub fn pixels() -> Option<(u16, u16)> {
    let (columns, rows) = terminal::size().ok()?;
    if columns == 0 || rows == 0 {
        return None;
    }
    let (width, height) = window_pixels()?;
    Some((
        (width / u32::from(columns)).max(1) as u16,
        (height / u32::from(rows)).max(1) as u16,
    ))
}

// CSI 14 t asks for the window in pixels; the reply is CSI 4 ; height ; width t.
fn window_pixels() -> Option<(u32, u32)> {
    let mut out = stdout();
    out.write_all("\u{1b}[14t".as_bytes()).ok()?;
    out.flush().ok()?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut reply = String::new();
        loop {
            let mut byte = [0u8; 1];
            match stdin.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let character = byte[0] as char;
                    reply.push(character);
                    if character == 't' {
                        break;
                    }
                }
            }
        }
        let _ = sender.send(reply);
    });
    let reply = receiver.recv_timeout(Duration::from_millis(300)).ok()?;
    let (_, tail) = reply.split_once("[4;")?;
    let (height, width) = tail.trim_end_matches('t').split_once(';')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}
