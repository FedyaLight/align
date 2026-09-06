//! Development readback check against a packaged align-aaf sidecar.
use std::{path::Path, sync::atomic::AtomicBool};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: aaf_read_smoke input.aaf")?;
    let result = align_decode::aaf::read_audio(Path::new(&path), &AtomicBool::new(false))?;
    println!("{result:#?}");
    Ok(())
}
