use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use speaches_companion::daemon::{DaemonResponse, HotkeyHandler};
use speaches_companion::dictation::{DictationController, Recorder, Transcriber};
use speaches_companion::inject::{
    format_transcript_for_injection, normalize_transcript_for_injection, TextInjector,
};
use speaches_companion::ipc::IpcCommand;
use speaches_companion::notification::{ErrorNotifier, DICTATION_ERROR_SUMMARY};

#[tokio::test]
async fn hotkey_up_records_transcribes_and_injects_text() {
    let recorder = FakeRecorder::new(PathBuf::from("/tmp/speaches-companion-test.wav"));
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
        vec![PathBuf::from("/tmp/speaches-companion-test.wav")]
    );
    assert_eq!(*injected.lock().unwrap(), vec!["hello window ".to_string()]);
}

#[tokio::test]
async fn trailing_space_can_be_disabled() {
    let recorder = FakeRecorder::new(PathBuf::from("/tmp/speaches-companion-test.wav"));
    let injector = FakeInjector::default();
    let injected = injector.injected.clone();
    let mut controller =
        DictationController::new(recorder, FakeTranscriber::new("  hello window\n"), injector)
            .with_append_space(false);

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap();

    assert_eq!(*injected.lock().unwrap(), vec!["hello window".to_string()]);
}

#[tokio::test]
async fn repeated_down_does_not_start_second_recording() {
    let recorder = FakeRecorder::new(PathBuf::from("/tmp/speaches-companion-test.wav"));
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
    let recorder = FakeRecorder::new(PathBuf::from("/tmp/speaches-companion-test.wav"));
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
    let recorder = FakeRecorder::new(PathBuf::from("/tmp/speaches-companion-test.wav"));
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

#[tokio::test]
async fn transcription_error_notifies_and_does_not_inject_text() {
    let recorder = FakeRecorder::new(PathBuf::from("/tmp/speaches-companion-test.wav"));
    let transcriber = FakeTranscriber::failing("connection refused");
    let injector = FakeInjector::default();
    let injected = injector.injected.clone();
    let notifier = FakeNotifier::default();
    let notifications = notifier.notifications.clone();
    let mut controller =
        DictationController::new_with_notifier(recorder, transcriber, injector, notifier);

    assert_eq!(
        controller
            .handle_hotkey(IpcCommand::HotkeyDown)
            .await
            .unwrap(),
        DaemonResponse::Started
    );
    let error = controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap_err();

    assert!(!controller.is_recording());
    assert!(error.to_string().contains("connection refused"));
    assert!(injected.lock().unwrap().is_empty());
    assert_eq!(
        *notifications.lock().unwrap(),
        vec![(
            DICTATION_ERROR_SUMMARY.to_string(),
            "Transcription failed: connection refused".to_string()
        )]
    );
}

#[test]
fn transcript_normalization_trims_outer_whitespace_only() {
    assert_eq!(
        normalize_transcript_for_injection("  hello world\n"),
        Some("hello world".to_string())
    );
    assert_eq!(normalize_transcript_for_injection("\n\t"), None);
}

#[test]
fn transcript_formatting_appends_optional_space_after_normalized_text() {
    assert_eq!(
        format_transcript_for_injection("  hello world\n", true),
        Some("hello world ".to_string())
    );
    assert_eq!(
        format_transcript_for_injection("  hello world\n", false),
        Some("hello world".to_string())
    );
    assert_eq!(format_transcript_for_injection("\n\t", true), None);
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
    result: Result<String, String>,
    paths: Arc<Mutex<Vec<PathBuf>>>,
}

impl FakeTranscriber {
    fn new(text: &str) -> Self {
        Self {
            result: Ok(text.to_string()),
            paths: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn failing(message: &str) -> Self {
        Self {
            result: Err(message.to_string()),
            paths: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Transcriber for FakeTranscriber {
    async fn transcribe(&self, audio_path: &Path) -> anyhow::Result<String> {
        self.paths.lock().unwrap().push(audio_path.to_path_buf());
        self.result.clone().map_err(anyhow::Error::msg)
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

#[derive(Clone, Default)]
struct FakeNotifier {
    notifications: Arc<Mutex<Vec<(String, String)>>>,
}

impl ErrorNotifier for FakeNotifier {
    fn notify_error(&self, summary: &str, body: &str) -> anyhow::Result<()> {
        self.notifications
            .lock()
            .unwrap()
            .push((summary.to_string(), body.to_string()));
        Ok(())
    }
}
