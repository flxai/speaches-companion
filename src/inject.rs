use std::ffi::CString;

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

    fn replace_tail(&self, erase_count: usize, text_suffix: &str) -> anyhow::Result<()> {
        self.erase_chars(erase_count)?;
        self.inject_text(text_suffix)
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

        let xdo = RawXdo::new()?;
        xdo.with_cleared_modifiers(|| xdo.enter_text(text, self.delay_microsecs))
            .context("libxdo text injection failed")
    }

    fn erase_chars(&self, count: usize) -> anyhow::Result<()> {
        if count == 0 {
            return Ok(());
        }

        self.replace_tail(count, "")
    }

    fn replace_tail(&self, erase_count: usize, text_suffix: &str) -> anyhow::Result<()> {
        if erase_count == 0 && text_suffix.is_empty() {
            return Ok(());
        }

        let xdo = RawXdo::new()?;
        xdo.with_cleared_modifiers(|| {
            for _ in 0..erase_count {
                xdo.send_keysequence("BackSpace", self.delay_microsecs)?;
            }
            xdo.enter_text(text_suffix, self.delay_microsecs)?;
            Ok(())
        })
        .context("libxdo text replacement failed")
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

    fn send_keysequence(&self, keysequence: &str, delay_microsecs: u32) -> anyhow::Result<()> {
        let keysequence =
            CString::new(keysequence).context("key sequence contains an interior NUL byte")?;
        let code = unsafe {
            libxdo_sys::xdo_send_keysequence_window(
                self.handle,
                libxdo_sys::CURRENTWINDOW,
                keysequence.as_ptr(),
                delay_microsecs,
            )
        };
        if code != 0 {
            bail!("failed to send key sequence through libxdo: error code {code}");
        }
        Ok(())
    }

    fn enter_text(&self, text: &str, delay_microsecs: u32) -> anyhow::Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        let text = CString::new(text).context("text contains an interior NUL byte")?;
        let code = unsafe {
            libxdo_sys::xdo_enter_text_window(
                self.handle,
                libxdo_sys::CURRENTWINDOW,
                text.as_ptr(),
                delay_microsecs,
            )
        };
        if code != 0 {
            bail!("failed to type text through libxdo: error code {code}");
        }
        Ok(())
    }

    fn with_cleared_modifiers(
        &self,
        operation: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut active_mods = std::ptr::null_mut();
        let mut active_mods_len = 0;
        let code = unsafe {
            libxdo_sys::xdo_get_active_modifiers(
                self.handle,
                &mut active_mods,
                &mut active_mods_len,
            )
        };
        if code != 0 {
            bail!("failed to read active X11 modifiers through libxdo: error code {code}");
        }

        let clear_result = self.clear_active_modifiers(active_mods, active_mods_len);
        let operation_result = clear_result.and_then(|()| operation());
        let restore_result = self.restore_active_modifiers(active_mods, active_mods_len);
        if !active_mods.is_null() {
            unsafe { libc::free(active_mods.cast()) };
        }

        operation_result?;
        restore_result?;
        Ok(())
    }

    fn clear_active_modifiers(
        &self,
        active_mods: *mut libxdo_sys::charcodemap_t,
        active_mods_len: i32,
    ) -> anyhow::Result<()> {
        if active_mods_len == 0 || active_mods.is_null() {
            return Ok(());
        }

        let code = unsafe {
            libxdo_sys::xdo_clear_active_modifiers(
                self.handle,
                libxdo_sys::CURRENTWINDOW,
                active_mods,
                active_mods_len,
            )
        };
        if code != 0 {
            bail!("failed to clear active X11 modifiers through libxdo: error code {code}");
        }
        Ok(())
    }

    fn restore_active_modifiers(
        &self,
        active_mods: *mut libxdo_sys::charcodemap_t,
        active_mods_len: i32,
    ) -> anyhow::Result<()> {
        if active_mods_len == 0 || active_mods.is_null() {
            return Ok(());
        }

        let code = unsafe {
            libxdo_sys::xdo_set_active_modifiers(
                self.handle,
                libxdo_sys::CURRENTWINDOW,
                active_mods,
                active_mods_len,
            )
        };
        if code != 0 {
            bail!("failed to restore active X11 modifiers through libxdo: error code {code}");
        }
        Ok(())
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

        let prefix_bytes = common_prefix_byte_len(&self.inserted_text, text);
        let erase_count = self.inserted_text[prefix_bytes..].chars().count();
        let text_suffix = &text[prefix_bytes..];
        self.injector.replace_tail(erase_count, text_suffix)?;
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

fn common_prefix_byte_len(left: &str, right: &str) -> usize {
    let mut prefix_bytes = 0;
    for (left_ch, right_ch) in left.chars().zip(right.chars()) {
        if left_ch != right_ch {
            break;
        }
        prefix_bytes += left_ch.len_utf8();
    }
    prefix_bytes
}

pub fn normalize_transcript_for_injection(transcript: &str) -> Option<String> {
    let trimmed = transcript.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn speculative_replacement_appends_shared_prefix_without_backspace() {
        let injector = FakeInjector::default();
        let operations = injector.operations.clone();
        let mut session = SpeculativeTextSession::start(injector).unwrap();

        session.replace_text("hel").unwrap();
        session.replace_text("hello win").unwrap();
        session.replace_text("hello window").unwrap();

        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                InjectOperation::Type("hel".to_string()),
                InjectOperation::Type("lo win".to_string()),
                InjectOperation::Type("dow".to_string()),
            ]
        );
    }

    #[test]
    fn speculative_replacement_backspaces_only_changed_tail() {
        let injector = FakeInjector::default();
        let operations = injector.operations.clone();
        let mut session = SpeculativeTextSession::start(injector).unwrap();

        session.replace_text("hello win").unwrap();
        session.replace_text("hello world").unwrap();

        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                InjectOperation::Type("hello win".to_string()),
                InjectOperation::Backspace(2),
                InjectOperation::Type("orld".to_string()),
            ]
        );
    }

    #[test]
    fn speculative_replacement_can_shrink_text() {
        let injector = FakeInjector::default();
        let operations = injector.operations.clone();
        let mut session = SpeculativeTextSession::start(injector).unwrap();

        session.replace_text("hello world").unwrap();
        session.replace_text("hello").unwrap();

        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                InjectOperation::Type("hello world".to_string()),
                InjectOperation::Backspace(6),
            ]
        );
    }

    #[test]
    fn speculative_replacement_counts_unicode_tail_chars() {
        let injector = FakeInjector::default();
        let operations = injector.operations.clone();
        let mut session = SpeculativeTextSession::start(injector).unwrap();

        session.replace_text("héllø win").unwrap();
        session.replace_text("héllø world").unwrap();

        assert_eq!(
            *operations.lock().unwrap(),
            vec![
                InjectOperation::Type("héllø win".to_string()),
                InjectOperation::Backspace(2),
                InjectOperation::Type("orld".to_string()),
            ]
        );
    }

    #[test]
    fn speculative_replacement_uses_single_tail_replacement_call() {
        let injector = TailReplacingInjector::default();
        let replacements = injector.replacements.clone();
        let mut session = SpeculativeTextSession::start(injector).unwrap();

        session.replace_text("💬").unwrap();
        session.replace_text("hello").unwrap();

        assert_eq!(
            *replacements.lock().unwrap(),
            vec![(0, "💬".to_string()), (1, "hello".to_string())]
        );
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum InjectOperation {
        Type(String),
        Backspace(usize),
    }

    #[derive(Clone, Default)]
    struct FakeInjector {
        operations: Arc<Mutex<Vec<InjectOperation>>>,
    }

    impl TextInjector for FakeInjector {
        fn inject_text(&self, text: &str) -> anyhow::Result<()> {
            if !text.is_empty() {
                self.operations
                    .lock()
                    .unwrap()
                    .push(InjectOperation::Type(text.to_string()));
            }
            Ok(())
        }

        fn erase_chars(&self, count: usize) -> anyhow::Result<()> {
            if count > 0 {
                self.operations
                    .lock()
                    .unwrap()
                    .push(InjectOperation::Backspace(count));
            }
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct TailReplacingInjector {
        replacements: Arc<Mutex<Vec<(usize, String)>>>,
    }

    impl TextInjector for TailReplacingInjector {
        fn inject_text(&self, text: &str) -> anyhow::Result<()> {
            self.replace_tail(0, text)
        }

        fn replace_tail(&self, erase_count: usize, text_suffix: &str) -> anyhow::Result<()> {
            self.replacements
                .lock()
                .unwrap()
                .push((erase_count, text_suffix.to_string()));
            Ok(())
        }
    }
}
