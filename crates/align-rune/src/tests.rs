use super::*;
use rune::{Env, Runtime};
use std::time::{Duration, Instant};

fn runtime() -> Runtime<State> {
    let mut rt = Runtime::new(
        init(),
        view::view,
        (1120.0, 700.0),
        Env {
            system_dark: true,
            scale: 1.0,
        },
    );
    rt.frame(0.0);
    rt
}
fn click(rt: &mut Runtime<State>, key: &str) {
    for _ in 0..90 {
        rt.frame(1.0 / 60.0);
    }
    rt.invalidate();
    for _ in 0..90 {
        rt.frame(1.0 / 60.0);
    }
    let (x, y) = rt
        .find(key)
        .unwrap_or_else(|| panic!("missing control {key}"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.1);
}
fn wait(rt: &mut Runtime<State>) {
    let start = Instant::now();
    while rt.state.busy() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "worker did not finish"
        );
        rt.update(|s, tx| {
            poll(s, start.elapsed().as_secs_f64(), tx);
        });
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(rt.state.data.error.is_none(), "{:?}", rt.state.data.error);
    rt.frame(0.2);
}

// The contract is the frontend's event delivery and command wiring, not DSP
// accuracy (owned by align-core). A real worker must reach the displayed state
// and export the same media without blocking or losing completion messages.
#[test]
fn dropped_recordings_reach_a_result_and_an_export_through_controls() {
    let root = std::env::temp_dir().join(format!("align-rune-workflow-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let mut rng = 7u32;
    let samples: Vec<i16> = (0..48000 * 25)
        .map(|_| {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            (rng as i16) / 3
        })
        .collect();
    for (name, offset) in [("recorder.wav", 0), ("camera.wav", 48000 * 3)] {
        let mut wav = hound::WavWriter::create(
            media.join(name),
            hound::WavSpec {
                channels: 1,
                sample_rate: 48000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for &sample in &samples[offset..offset + 48000 * 20] {
            wav.write_sample(sample).unwrap();
        }
        wav.finalize().unwrap();
    }
    let mut rt = runtime();
    rt.drop_files(vec![media], None);
    rt.frame(0.1);
    assert_eq!(rt.state.data.inputs.len(), 1);
    click(&mut rt, "sync");
    wait(&mut rt);
    assert_eq!(rt.state.data.clips.len(), 2);
    assert!(rt.state.data.can_export());
    assert!(!rt.state.data.lanes.is_empty());
    click(&mut rt, "export");
    rt.state.data.export_dir = Some(root.join("export"));
    rt.state.data.export_selected = [align_ui::state::ExportTarget::Premiere]
        .into_iter()
        .collect();
    rt.frame(0.1);
    click(&mut rt, "write-export");
    wait(&mut rt);
    assert_eq!(rt.state.data.operation, Operation::Exported);
    let files = &rt.state.data.exported_files;
    let xml = files
        .iter()
        .find(|p| p.extension().is_some_and(|e| e == "xml"))
        .expect("exported XML");
    let xml = std::fs::read_to_string(xml).unwrap();
    assert!(xml.contains("recorder.wav") && xml.contains("camera.wav"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancel_then_clear_discards_late_worker_results() {
    let mut rt = runtime();
    rt.state.data.add_paths(vec!["missing.wav".into()]);
    rt.frame(0.1);
    click(&mut rt, "sync");
    click(&mut rt, "sync"); // The same control becomes Cancel.
    assert!(!rt.state.busy());
    click(&mut rt, "clear");
    std::thread::sleep(Duration::from_millis(50));
    poll(&mut rt.state, 1.0, &mut Tx::default());
    assert!(rt.state.data.clips.is_empty());
    assert!(rt.state.data.result.is_none());
    assert!(rt.state.data.error.is_none());
}
