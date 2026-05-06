use notify_rust::{Notification, Timeout, Urgency};

pub const DICTATION_ERROR_SUMMARY: &str = "speaches-scribe dictation failed";
pub const DICTATION_PARTIAL_SUMMARY: &str = "speaches-scribe dictating";
pub const DICTATION_FINAL_SUMMARY: &str = "speaches-scribe dictation";
pub const HOTKEY_ERROR_SUMMARY: &str = "speaches-scribe hotkey failed";
const TRANSCRIPT_NOTIFICATION_ID: u32 = 0x7472_6563;

pub trait ErrorNotifier: Send + Sync {
    fn notify_error(&self, summary: &str, body: &str) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopErrorNotifier;

impl ErrorNotifier for NoopErrorNotifier {
    fn notify_error(&self, _summary: &str, _body: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct DesktopErrorNotifier;

impl ErrorNotifier for DesktopErrorNotifier {
    fn notify_error(&self, summary: &str, body: &str) -> anyhow::Result<()> {
        Notification::new()
            .appname("speaches-scribe")
            .summary(summary)
            .body(body)
            .urgency(Urgency::Critical)
            .show()?;
        Ok(())
    }
}

pub fn dictation_error_body(stage: &str, error: &anyhow::Error) -> String {
    format!("{stage}: {error:#}")
}

pub trait TranscriptNotifier: Clone + Send + Sync + 'static {
    fn notify_listening(&self) -> anyhow::Result<()>;
    fn notify_partial(&self, transcript: &str) -> anyhow::Result<()>;
    fn notify_final(&self, transcript: &str) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopTranscriptNotifier;

impl TranscriptNotifier for NoopTranscriptNotifier {
    fn notify_listening(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn notify_partial(&self, _transcript: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn notify_final(&self, _transcript: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct DesktopTranscriptNotifier;

impl TranscriptNotifier for DesktopTranscriptNotifier {
    fn notify_listening(&self) -> anyhow::Result<()> {
        show_transcript_notification(
            DICTATION_PARTIAL_SUMMARY,
            "listening...",
            Timeout::Milliseconds(2_000),
        )
    }

    fn notify_partial(&self, transcript: &str) -> anyhow::Result<()> {
        show_transcript_notification(
            DICTATION_PARTIAL_SUMMARY,
            &truncate_notification_body(transcript),
            Timeout::Milliseconds(2_000),
        )
    }

    fn notify_final(&self, transcript: &str) -> anyhow::Result<()> {
        show_transcript_notification(
            DICTATION_FINAL_SUMMARY,
            &truncate_notification_body(transcript),
            Timeout::Milliseconds(3_000),
        )
    }
}

fn show_transcript_notification(summary: &str, body: &str, timeout: Timeout) -> anyhow::Result<()> {
    Notification::new()
        .appname("speaches-scribe")
        .id(TRANSCRIPT_NOTIFICATION_ID)
        .summary(summary)
        .body(body)
        .urgency(Urgency::Normal)
        .timeout(timeout)
        .show()?;
    Ok(())
}

fn truncate_notification_body(body: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut chars = body.chars();
    let truncated: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}
