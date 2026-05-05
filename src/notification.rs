use notify_rust::{Notification, Urgency};

pub const DICTATION_ERROR_SUMMARY: &str = "trec dictation failed";

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
            .appname("trec")
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
