//! Diagnostic waveform sampling across the full overlap, outside cached analysis.
//! cargo run --release -p align-decode --example drift_audit -- result.json
//! Render timing: drift_audit render source.wav output.wav seconds ratio
use align_core::AudioAnalysisSource;
use serde_json::Value;
use std::{collections::HashMap, path::Path};

fn seconds(v: &Value) -> f64 {
    v["value"].as_f64().unwrap() / v["timescale"].as_f64().unwrap()
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 2 && !(args.len() == 6 && args[1] == "render") {
        eprintln!("Usage: drift_audit result.json | render source output seconds ratio");
        std::process::exit(2);
    }
    let backend = align_decode::default_backend();
    if args[1] == "render" {
        let duration: f64 = args[4].parse().unwrap();
        let ratio: f64 = args[5].parse().unwrap();
        let start = std::time::Instant::now();
        align_decode::render::render_drift(
            &*backend,
            Path::new(&args[2]),
            Path::new(&args[3]),
            &[(0.0, 0.0), (duration, duration * ratio)],
        )
        .unwrap();
        eprintln!("render_seconds={:.3}", start.elapsed().as_secs_f64());
        return;
    }
    let result: Value = serde_json::from_slice(&std::fs::read(&args[1]).unwrap()).unwrap();
    let clips: HashMap<_, _> = result["project"]["clips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap(), c))
        .collect();
    println!("left,right,left_seconds,offset_ms,quality");
    for pair in result["matches"].as_array().unwrap() {
        let left = clips[pair["left"].as_str().unwrap()];
        let right = clips[pair["right"].as_str().unwrap()];
        let lp = Path::new(left["url"].as_str().unwrap());
        let rp = Path::new(right["url"].as_str().unwrap());
        let rate = 1.0 + pair["driftPPM"].as_f64().unwrap() / 1e6;
        let offset = seconds(&pair["offset"]);
        let lo = 5.0_f64.max((5.0 - offset) / rate);
        let hi = (seconds(&left["duration"]) - 5.0)
            .min((seconds(&right["duration"]) - 5.0 - offset) / rate);
        for i in 0..15 {
            let center = lo + (hi - lo) * (i as f64 + 0.5) / 15.0;
            let (ls, l) = backend
                .decode_window_16k(lp, center - 2.1, 4.2, AudioAnalysisSource::Automatic)
                .unwrap();
            let (rs, r) = backend
                .decode_window_16k(
                    rp,
                    center * rate + offset - 2.1,
                    4.2,
                    AudioAnalysisSource::Automatic,
                )
                .unwrap();
            if let Some(a) = align_core::gccphat::align(&l, &r, 4000) {
                println!(
                    "{},{},{:.6},{:.6},{:.4}",
                    lp.file_name().unwrap().to_string_lossy(),
                    rp.file_name().unwrap().to_string_lossy(),
                    ls + 2.048,
                    (rs - ls + a.lag_samples / 16000.0) * 1000.0,
                    a.quality
                );
            }
        }
    }
}
