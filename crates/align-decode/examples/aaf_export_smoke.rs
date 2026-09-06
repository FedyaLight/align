//! End-to-end development AAF export from a saved synchronization result.
use std::{path::Path, sync::atomic::AtomicBool};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 {
        return Err("usage: aaf_export_smoke result.json output-directory".into());
    }
    let result: align_core::SyncResult = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let timeline = align_core::export::ExportTimeline::from_result(&result, true)?;
    let pipeline = align_decode::pipeline::Pipeline::default_backend();
    let artifacts = align_decode::export::export_prepared(
        align_decode::export::ExportRequest {
            backend: pipeline.backend(),
            timeline: &timeline,
            directory: Path::new(&args[2]),
            formats: &[align_core::export::TimelineExportFormat::Aaf],
            correct_drift: true,
            include_replaced_sequence: false,
            include_media_files: false,
            cancel: &AtomicBool::new(false),
        },
        None,
    )?;
    println!("{}", serde_json::to_string(&artifacts)?);
    Ok(())
}
