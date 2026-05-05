use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use trec::daemon::{DaemonResponse, HotkeyHandler};
use trec::inject::TextInjector;
use trec::ipc::IpcCommand;
use trec::notification::{ErrorNotifier, TranscriptNotifier, DICTATION_ERROR_SUMMARY};
use trec::streaming::{
    LiveTranscriber, LiveTranscriptUpdate, LiveTranscriptionSession, StreamingDictationController,
};

#[tokio::test]
async fn streaming_hotkey_injects_final_text_and_reports_partials() {
    let transcriber =
        FakeLiveTranscriber::new(["hel", "hello win"], Ok("  hello window\n".to_string()));
    let starts = transcriber.starts.clone();
    let stops = transcriber.stops.clone();
    let injector = FakeInjector::default();
    let injected = injector.injected.clone();
    let transcript_notifier = FakeTranscriptNotifier::default();
    let transcript_events = transcript_notifier.events.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        transcript_notifier,
        FakeErrorNotifier::default(),
    );

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
    assert_eq!(*injected.lock().unwrap(), vec!["hello window".to_string()]);
    assert_eq!(
        *transcript_events.lock().unwrap(),
        vec![
            TranscriptNotice::Listening,
            TranscriptNotice::Partial("hel".to_string()),
            TranscriptNotice::Partial("hello win".to_string()),
            TranscriptNotice::Final("hello window".to_string()),
        ]
    );
}

#[tokio::test]
async fn streaming_uses_last_partial_when_final_is_empty() {
    let transcriber = FakeLiveTranscriber::new(["fallback text"], Ok(" \n".to_string()));
    let injector = FakeInjector::default();
    let injected = injector.injected.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        FakeErrorNotifier::default(),
    );

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap();

    assert_eq!(*injected.lock().unwrap(), vec!["fallback text".to_string()]);
}

#[tokio::test]
async fn streaming_stop_error_notifies_and_does_not_inject() {
    let transcriber = FakeLiveTranscriber::new(["partial"], Err("connection refused".to_string()));
    let injector = FakeInjector::default();
    let injected = injector.injected.clone();
    let error_notifier = FakeErrorNotifier::default();
    let errors = error_notifier.errors.clone();
    let mut controller = StreamingDictationController::new_with_notifiers(
        transcriber,
        injector,
        FakeTranscriptNotifier::default(),
        error_notifier,
    );

    controller
        .handle_hotkey(IpcCommand::HotkeyDown)
        .await
        .unwrap();
    let error = controller
        .handle_hotkey(IpcCommand::HotkeyUp)
        .await
        .unwrap_err();

    assert!(!controller.is_recording());
    assert!(error.to_string().contains("connection refused"));
    assert!(injected.lock().unwrap().is_empty());
    assert_eq!(
        *errors.lock().unwrap(),
        vec![(
            DICTATION_ERROR_SUMMARY.to_string(),
            "Streaming transcription failed: connection refused".to_string()
        )]
    );
}

#[tokio::test]
async fn repeated_streaming_down_does_not_start_second_session() {
    let transcriber = FakeLiveTranscriber::new(Vec::<String>::new(), Ok("hello".to_string()));
    let starts = transcriber.starts.clone();
    let mut controller = StreamingDictationController::new(transcriber, FakeInjector::default());

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

#[derive(Clone)]
struct FakeSession;

#[derive(Clone)]
struct FakeLiveTranscriber {
    updates: Vec<String>,
    final_result: Result<String, String>,
    starts: Arc<Mutex<usize>>,
    stops: Arc<Mutex<usize>>,
}

impl FakeLiveTranscriber {
    fn new(
        updates: impl IntoIterator<Item = impl Into<String>>,
        final_result: Result<String, String>,
    ) -> Self {
        Self {
            updates: updates.into_iter().map(Into::into).collect(),
            final_result,
            starts: Arc::new(Mutex::new(0)),
            stops: Arc::new(Mutex::new(0)),
        }
    }
}

#[async_trait]
impl LiveTranscriber for FakeLiveTranscriber {
    type Session = FakeSession;

    async fn start(&self) -> anyhow::Result<LiveTranscriptionSession<Self::Session>> {
        *self.starts.lock().unwrap() += 1;
        let (updates_tx, updates_rx) = tokio::sync::mpsc::channel(8);
        for transcript in &self.updates {
            updates_tx
                .send(LiveTranscriptUpdate {
                    transcript: transcript.clone(),
                })
                .await
                .unwrap();
        }
        drop(updates_tx);
        Ok(LiveTranscriptionSession {
            session: FakeSession,
            updates: updates_rx,
        })
    }

    async fn stop(&self, _session: Self::Session) -> anyhow::Result<String> {
        *self.stops.lock().unwrap() += 1;
        self.final_result.clone().map_err(anyhow::Error::msg)
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum TranscriptNotice {
    Listening,
    Partial(String),
    Final(String),
}

#[derive(Clone, Default)]
struct FakeTranscriptNotifier {
    events: Arc<Mutex<Vec<TranscriptNotice>>>,
}

impl TranscriptNotifier for FakeTranscriptNotifier {
    fn notify_listening(&self) -> anyhow::Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(TranscriptNotice::Listening);
        Ok(())
    }

    fn notify_partial(&self, transcript: &str) -> anyhow::Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(TranscriptNotice::Partial(transcript.to_string()));
        Ok(())
    }

    fn notify_final(&self, transcript: &str) -> anyhow::Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(TranscriptNotice::Final(transcript.to_string()));
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FakeErrorNotifier {
    errors: Arc<Mutex<Vec<(String, String)>>>,
}

impl ErrorNotifier for FakeErrorNotifier {
    fn notify_error(&self, summary: &str, body: &str) -> anyhow::Result<()> {
        self.errors
            .lock()
            .unwrap()
            .push((summary.to_string(), body.to_string()));
        Ok(())
    }
}
