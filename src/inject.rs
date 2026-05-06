use anyhow::{bail, Context};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusedWindow(pub u64);

pub trait TextInjector: Send + Sync {
    fn inject_text(&self, text: &str) -> anyhow::Result<()>;

    fn erase_chars(&self, count: usize) -> anyhow::Result<()> {
        if count == 0 {
            return Ok(());
        }

        bail!("text injector does not support erasing {count} characters")
    }

    fn focused_window(&self) -> anyhow::Result<Option<FocusedWindow>> {
        Ok(None)
    }
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

    fn erase_chars(&self, count: usize) -> anyhow::Result<()> {
        if count == 0 {
            return Ok(());
        }

        let xdo = libxdo::XDo::new(None)
            .map_err(|error| anyhow::anyhow!("failed to connect to X11 through libxdo: {error}"))?;
        for _ in 0..count {
            xdo.send_keysequence("BackSpace", self.delay_microsecs)
                .map_err(|error| {
                    anyhow::anyhow!("failed to send BackSpace through libxdo: {error}")
                })
                .context("libxdo text erasure failed")?;
        }
        Ok(())
    }

    fn focused_window(&self) -> anyhow::Result<Option<FocusedWindow>> {
        let xdo = RawXdo::new()?;
        let mut window = unsafe { std::mem::zeroed() };
        let code = unsafe { libxdo_sys::xdo_get_active_window(xdo.handle, &mut window) };
        if code != 0 {
            bail!("failed to read active X11 window through libxdo: error code {code}");
        }
        Ok(Some(FocusedWindow(window)))
    }
}

struct RawXdo {
    handle: *mut libxdo_sys::xdo_t,
}

impl RawXdo {
    fn new() -> anyhow::Result<Self> {
        let handle = unsafe { libxdo_sys::xdo_new(std::ptr::null()) };
        if handle.is_null() {
            bail!("failed to connect to X11 through libxdo");
        }
        Ok(Self { handle })
    }
}

impl Drop for RawXdo {
    fn drop(&mut self) {
        unsafe { libxdo_sys::xdo_free(self.handle) };
    }
}

pub struct SpeculativeTextSession<I>
where
    I: TextInjector,
{
    injector: I,
    target_window: Option<FocusedWindow>,
    inserted_text: String,
    aborted: bool,
}

impl<I> SpeculativeTextSession<I>
where
    I: TextInjector,
{
    pub fn start(injector: I) -> anyhow::Result<Self> {
        let target_window = injector.focused_window()?;
        Ok(Self {
            injector,
            target_window,
            inserted_text: String::new(),
            aborted: false,
        })
    }

    pub fn replace_text(&mut self, text: &str) -> anyhow::Result<bool> {
        if text == self.inserted_text {
            return Ok(false);
        }
        self.ensure_target_is_still_focused()?;

        let erase_count = self.inserted_text.chars().count();
        self.injector.erase_chars(erase_count)?;
        self.injector.inject_text(text)?;
        self.inserted_text = text.to_string();
        Ok(true)
    }

    pub fn inserted_text(&self) -> &str {
        &self.inserted_text
    }

    pub fn abort(&mut self) {
        self.aborted = true;
    }

    fn ensure_target_is_still_focused(&mut self) -> anyhow::Result<()> {
        if self.aborted {
            bail!("dictation target is no longer safe for replacement");
        }

        let Some(target_window) = self.target_window else {
            return Ok(());
        };
        let Some(current_window) = self.injector.focused_window()? else {
            self.abort();
            bail!("active X11 window is unavailable; aborting replacement");
        };
        if current_window != target_window {
            self.abort();
            bail!(
                "focused X11 window changed from {} to {}; aborting replacement",
                target_window.0,
                current_window.0
            );
        }

        Ok(())
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
