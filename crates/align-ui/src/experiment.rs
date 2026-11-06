//! Opt-in inputs for reproducible desktop comparisons. Never enabled during normal use.
use crate::{scroll::ScrollPort, state::DesktopState};

pub fn load<S: ScrollPort>(data: &mut DesktopState<S>) {
    if let Some(path) = std::env::var_os("ALIGN_EXPERIMENT_RESULT") {
        match std::fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
        {
            Ok(result) => data.apply_result(result),
            Err(error) => data.error = Some(error),
        }
    }
    if let Some(path) = std::env::var_os("ALIGN_EXPERIMENT_INPUT") {
        data.add_paths(vec![path.into()]);
        if data.sequence_picker.is_some() {
            data.choose_all_sequences();
        }
    }
}

pub fn save(result: &align_core::SyncResult) {
    if let Some(path) = std::env::var_os("ALIGN_EXPERIMENT_SAVE") {
        match serde_json::to_vec_pretty(result)
            .map_err(|e| e.to_string())
            .and_then(|b| std::fs::write(path, b).map_err(|e| e.to_string()))
        {
            Ok(()) => {}
            Err(e) => eprintln!("Could not save experiment result: {e}"),
        }
    }
}

pub fn zoom(seconds: f64) -> f64 {
    0.5 + 0.5 * (seconds * std::f64::consts::TAU / 4.0).sin()
}

pub fn pipeline() -> align_decode::pipeline::Pipeline {
    match std::env::var_os("ALIGN_EXPERIMENT_CACHE") {
        Some(path) => align_decode::pipeline::Pipeline::new_in(
            align_decode::default_backend_kind(),
            Some(path.into()),
        ),
        None => align_decode::pipeline::Pipeline::default_backend(),
    }
}
