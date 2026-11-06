//! Experimental native Rune frontend. The synchronization model is shared with GPUI.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod jobs;
mod view;

use align_ui::state::{AppData, Operation};
use rune::{AppSpec, Key, KeyEvent, Tx, Waker};
use std::sync::mpsc;

pub struct State {
    data: AppData,
    sender: mpsc::SyncSender<jobs::Event>,
    receiver: mpsc::Receiver<jobs::Event>,
    wake: Waker,
    settings: bool,
    dark: bool,
    started: bool,
}

fn init() -> State {
    let (sender, receiver) = mpsc::sync_channel(64);
    let mut data = AppData::new();
    align_ui::experiment::load(&mut data);
    State {
        data,
        sender,
        receiver,
        wake: std::sync::Arc::new(|| {}),
        settings: false,
        dark: true,
        started: false,
    }
}

impl State {
    fn busy(&self) -> bool {
        matches!(
            self.data.operation,
            Operation::Synchronizing | Operation::Exporting | Operation::Repairing
        )
    }
    fn cancel(&mut self) {
        let operation = self.data.operation;
        self.data.cancel_run();
        if operation == Operation::Synchronizing {
            self.data.restore_after_sync_cancel();
        } else if operation == Operation::Exporting {
            self.data.operation = Operation::Ready;
            self.data.status = "Export cancelled.".into();
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        self.data
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn setup(s: &mut State, wake: Waker) {
    s.wake = wake;
}

fn poll(s: &mut State, time: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    if !s.started {
        s.started = true;
        if std::env::var_os("ALIGN_EXPERIMENT_SYNC").is_some() {
            s.synchronize();
            changed = true;
        }
    }
    while let Ok(event) = s.receiver.try_recv() {
        changed |= s.apply(event);
    }
    if std::env::var_os("ALIGN_EXPERIMENT_ANIMATE").is_some() {
        s.data.zoom_level = align_ui::experiment::zoom(time);
        tx.wake_after(std::time::Duration::from_secs_f64(1.0 / 60.0));
        changed = true;
    }
    if changed {
        tx.instant();
    }
    changed
}

fn on_key(s: &mut State, key: &KeyEvent, tx: &mut Tx) -> bool {
    match key.key {
        Key::Command('q') => {
            s.cancel();
            tx.quit();
        }
        Key::Command('o') => view::add_file(s, tx),
        Key::Command('r') if !s.busy() => s.synchronize(),
        Key::Command('e') if s.data.can_export() => s.data.show_export = !s.data.show_export,
        _ => return false,
    }
    true
}

fn main() {
    struct SessionCleanup;
    impl Drop for SessionCleanup {
        fn drop(&mut self) {
            align_decode::media_assets::cleanup();
        }
    }
    let _cleanup = SessionCleanup;
    rune::run(AppSpec::<State> {
        scenes: &[
            ("settings", |s| s.settings = true),
            ("export", |s| s.data.show_export = true),
        ],
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        min_size: Some((900.0, 600.0)),
        ..AppSpec::new("Align — Rune experiment", (1120.0, 700.0), init, view::view)
    });
}

#[cfg(test)]
mod tests;
