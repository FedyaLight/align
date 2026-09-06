//! Development smoke check against a packaged align-aaf sidecar.
use std::{path::Path, sync::atomic::AtomicBool};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 {
        return Err("usage: aaf_bridge_smoke manifest.json output.aaf".into());
    }
    align_decode::aaf::write_audio(
        Path::new(&args[1]),
        Path::new(&args[2]),
        &AtomicBool::new(false),
    )?;
    Ok(())
}
