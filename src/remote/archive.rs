use std::{
    fs::File,
    io::{self, ErrorKind},
    path::PathBuf,
};

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use tokio::task::spawn_blocking;

// A capsule crosses a network, so what is written into it is compressed. What the
// isolation layer is handed is a plain archive either way: the bytes of a file sit
// at an offset the tar header names, and that is what lets a tree be carried out or
// put in file by file instead of being assembled on a disk first.
pub async fn pack(plain: PathBuf, packed: PathBuf) -> io::Result<u64> {
    spawn_blocking(move || {
        let mut source = File::open(&plain)?;
        let mut encoder = GzEncoder::new(File::create(&packed)?, Compression::default());
        let bytes = io::copy(&mut source, &mut encoder)?;
        encoder.finish()?;
        Ok(bytes)
    })
    .await
    .map_err(io::Error::other)?
}

pub async fn expand(packed: PathBuf, plain: PathBuf) -> io::Result<u64> {
    spawn_blocking(move || {
        let mut decoder = GzDecoder::new(File::open(&packed)?);
        let mut sink = File::create(&plain)?;
        let bytes = io::copy(&mut decoder, &mut sink)?;
        // Provisioning reads this back by offset, and it is about to be read by a
        // different handle, so it is on disk before the transfer counts as landed.
        sink.sync_all()?;
        if bytes == 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "the arriving archive holds nothing",
            ));
        }
        Ok(bytes)
    })
    .await
    .map_err(io::Error::other)?
}
