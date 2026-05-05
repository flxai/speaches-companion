use anyhow::Context;

pub trait TextInjector: Send + Sync {
    fn inject_text(&self, text: &str) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, Default)]
pub struct LibXdoTextInjector {
    delay_microsecs: u32,
}

impl LibXdoTextInjector {
    pub fn new(delay_microsecs: u32) -> Self {
        Self { delay_microsecs }
    }
}

impl TextInjector for LibXdoTextInjector {
    fn inject_text(&self, text: &str) -> anyhow::Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        let xdo = libxdo::XDo::new(None)
            .map_err(|error| anyhow::anyhow!("failed to connect to X11 through libxdo: {error}"))?;
        xdo.enter_text(text, self.delay_microsecs)
            .map_err(|error| anyhow::anyhow!("failed to type text through libxdo: {error}"))
            .context("libxdo text injection failed")
    }
}

pub fn normalize_transcript_for_injection(transcript: &str) -> Option<String> {
    let trimmed = transcript.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
