use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use trec::daemon::{DaemonResponse, HotkeyHandler};
use trec::dictation::{DictationController, Recorder, Transcriber};
use trec::inject::{normalize_transcript_for_injection, TextInjector};
use trec::ipc::IpcCommand;

#[tokio::test]
async fn hotkey_up_records_transcribes_and_injects_text() {
    let recorder = FakeRecorder::new(PathBuf::from("/tmp/trec-test.wav"));
    let transcriber = FakeTranscriber::new("  hello window\n");
    let injector = FakeInjector::default();
    let injected = injector.injected.clone();
    let transcribed_paths = transcriber.paths.clone();
    let starts = recorder.starts.clone();
    let stops = recorder.stops.clone();
    let mut controller = DictationController::new(recorder, transcriber, injector);

    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyDown)
            .await
            .unwrap(),
        DaemonResponse::Started
    );
    assert!(controller.is_recording());
    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyUp)
            .await
            .unwrap(),
        DaemonResponse::Stopped
    );

    assert!(!controller.is_recording());
    assert_eq!(*starts.lock().unwrap(), 1);
    assert_eq!(*stops.lock().unwrap(), 1);
    assert_eq!(
        *transcribed_paths.lock().unwrap(),
        vec![PathBuf::from("/tmp/trec-test.wav")]
    );
    assert_eq!(*injected.lock().unwrap(), vec!["hello window".to_string()]);
}

#[tokio::test]
async fn repeated_down_does_not_start_second_recording() {
    let recorder = FakeRecorder::new(PathBuf::from("/tmp/trec-test.wav"));
    let starts = recorder.starts.clone();
    let mut controller = DictationController::new(
        recorder,
        FakeTranscriber::new("hello"),
        FakeInjector::default(),
    );

    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyDown)
            .await
            .unwrap(),
        DaemonResponse::Started
    );
    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyDown)
            .await
            .unwrap(),
        DaemonResponse::AlreadyRecording
    );

    assert_eq!(*starts.lock().unwrap(), 1);
}

#[tokio::test]
async fn up_while_idle_is_a_noop() {
    let recorder = FakeRecorder::new(PathBuf::from("/tmp/trec-test.wav"));
    let stops = recorder.stops.clone();
    let injector = FakeInjector::default();
    let injected = injector.injected.clone();
    let mut controller =
        DictationController::new(recorder, FakeTranscriber::new("hello"), injector);

    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyUp)
            .await
            .unwrap(),
        DaemonResponse::AlreadyIdle
    );

    assert_eq!(*stops.lock().unwrap(), 0);
    assert!(injected.lock().unwrap().is_empty());
}

#[tokio::test]
async fn empty_transcript_does_not_inject_text() {
    let recorder = FakeRecorder::new(PathBuf::from("/tmp/trec-test.wav"));
    let injector = FakeInjector::default();
    let injected = injector.injected.clone();
    let mut controller =
        DictationController::new(recorder, FakeTranscriber::new(" \n\t "), injector);

    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyDown)
            .await
            .unwrap(),
        DaemonResponse::Started
    );
    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyUp)
            .await
            .unwrap(),
        DaemonResponse::Stopped
    );

    assert!(injected.lock().unwrap().is_empty());
}

#[test]
fn transcript_normalization_trims_outer_whitespace_only() {
    assert_eq!(
        normalize_transcript_for_injection("  hello world\n"),
        Some("hello world".to_string())
    );
    assert_eq!(normalize_transcript_for_injection("\n\t"), None);
}

#[derive(Clone)]
struct FakeRecording;

#[derive(Clone)]
struct FakeRecorder {
    path: PathBuf,
    starts: Arc<Mutex<usize>>,
    stops: Arc<Mutex<usize>>,
}

impl FakeRecorder {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            starts: Arc::new(Mutex::new(0)),
            stops: Arc::new(Mutex::new(0)),
        }
    }
}

#[async_trait]
impl Recorder for FakeRecorder {
    type Recording = FakeRecording;

    async fn start(&self) -> anyhow::Result<Self::Recording> {
        *self.starts.lock().unwrap() += 1;
        Ok(FakeRecording)
    }

    async fn stop(&self, _recording: Self::Recording) -> anyhow::Result<PathBuf> {
        *self.stops.lock().unwrap() += 1;
        Ok(self.path.clone())
    }
}

#[derive(Clone)]
struct FakeTranscriber {
    text: String,
    paths: Arc<Mutex<Vec<PathBuf>>>,
}

impl FakeTranscriber {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            paths: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Transcriber for FakeTranscriber {
    async fn transcribe(&self, audio_path: &Path) -> anyhow::Result<String> {
        self.paths.lock().unwrap().push(audio_path.to_path_buf());
        Ok(self.text.clone())
    }
}

#[derive(Clone, Default)]
struct FakeInjector {
    injected: Arc<Mutex<Vec<String>>>,
}

impl TextInjector for FakeInjector {
    fn inject_text(&self, text: &str) -> anyhow::Result<()> {
        self.injected.lock().unwrap().push(text.to_string());
        Ok(())
    }
}
