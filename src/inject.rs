use std::ffi::CString;
use std::io::Write;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context};
use serde_json::Value;

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

#[derive(Debug, Clone)]
pub struct DesktopTextInjector {
    delay_microsecs: u32,
}

impl DesktopTextInjector {
    pub fn new(delay_microsecs: u32) -> Self {
        Self { delay_microsecs }
    }

    fn backend(&self) -> DesktopTextBackend {
        if let Some(injector) = SwayTextInjector::detect(self.delay_microsecs) {
            DesktopTextBackend::Sway(injector)
        } else {
            DesktopTextBackend::X11(LibXdoTextInjector::new(self.delay_microsecs))
        }
    }
}

impl Default for DesktopTextInjector {
    fn default() -> Self {
        Self::new(0)
    }
}

enum DesktopTextBackend {
    Sway(SwayTextInjector),
    X11(LibXdoTextInjector),
}

impl TextInjector for DesktopTextBackend {
    fn inject_text(&self, text: &str) -> anyhow::Result<()> {
        match self {
            Self::Sway(injector) => injector.inject_text(text),
            Self::X11(injector) => injector.inject_text(text),
        }
    }

    fn erase_chars(&self, count: usize) -> anyhow::Result<()> {
        match self {
            Self::Sway(injector) => injector.erase_chars(count),
            Self::X11(injector) => injector.erase_chars(count),
        }
    }

    fn replace_tail(&self, erase_count: usize, text_suffix: &str) -> anyhow::Result<()> {
        match self {
            Self::Sway(injector) => injector.replace_tail(erase_count, text_suffix),
            Self::X11(injector) => injector.replace_tail(erase_count, text_suffix),
        }
    }

    fn focused_window(&self) -> anyhow::Result<Option<FocusedWindow>> {
        match self {
            Self::Sway(injector) => injector.focused_window(),
            Self::X11(injector) => injector.focused_window(),
        }
    }
}

impl TextInjector for DesktopTextInjector {
    fn inject_text(&self, text: &str) -> anyhow::Result<()> {
        self.backend().inject_text(text)
    }

    fn erase_chars(&self, count: usize) -> anyhow::Result<()> {
        self.backend().erase_chars(count)
    }

    fn replace_tail(&self, erase_count: usize, text_suffix: &str) -> anyhow::Result<()> {
        self.backend().replace_tail(erase_count, text_suffix)
    }

    fn focused_window(&self) -> anyhow::Result<Option<FocusedWindow>> {
        self.backend().focused_window()
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

#[derive(Debug, Clone)]
pub struct SwayTextInjector {
    delay_millis: u32,
    sway_socket: PathBuf,
    wayland_display: Option<String>,
    swaymsg_path: PathBuf,
    wtype_path: PathBuf,
}

impl SwayTextInjector {
    pub fn detect(delay_microsecs: u32) -> Option<Self> {
        let sway_socket = default_sway_socket_path()?;
        let wayland_display =
            default_wayland_display().or_else(|| wayland_display_from_sway_process(&sway_socket));
        Some(Self {
            delay_millis: delay_microsecs.div_ceil(1000),
            sway_socket,
            wayland_display,
            swaymsg_path: PathBuf::from("swaymsg"),
            wtype_path: PathBuf::from("wtype"),
        })
    }

    fn swaymsg_command(&self) -> Command {
        let mut command = Command::new(&self.swaymsg_path);
        command.arg("-s").arg(&self.sway_socket);
        command
    }

    fn wtype_command(&self) -> Command {
        let mut command = Command::new(&self.wtype_path);
        if let Some(wayland_display) = self.wayland_display.as_ref() {
            command.env("WAYLAND_DISPLAY", wayland_display);
        }
        command
    }

    fn add_wtype_timing_args(&self, command: &mut Command) {
        if self.delay_millis > 0 {
            let delay = self.delay_millis.to_string();
            command.arg("-s").arg(&delay);
            command.arg("-d").arg(delay);
        }
    }

    fn add_wtype_replacement_args(
        &self,
        command: &mut Command,
        erase_count: usize,
        has_text_suffix: bool,
    ) {
        self.add_wtype_timing_args(command);
        for _ in 0..erase_count {
            command.arg("-k").arg("BackSpace");
        }
        if has_text_suffix {
            command.arg("-");
        }
    }

    fn run_wtype_with_text(&self, text: &str) -> anyhow::Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        let mut command = self.wtype_command();
        self.add_wtype_replacement_args(&mut command, 0, true);
        command.stdin(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start {}", self.wtype_path.display()))?;
        let mut stdin = child.stdin.take().context("failed to open wtype stdin")?;
        stdin
            .write_all(text.as_bytes())
            .context("failed to send text to wtype")?;
        drop(stdin);
        let status = child.wait().context("failed to wait for wtype")?;
        if !status.success() {
            bail!("wtype failed with status {status}");
        }
        Ok(())
    }

    fn run_wtype_replacement(&self, erase_count: usize, text_suffix: &str) -> anyhow::Result<()> {
        if erase_count == 0 && text_suffix.is_empty() {
            return Ok(());
        }

        let mut command = self.wtype_command();
        self.add_wtype_replacement_args(&mut command, erase_count, !text_suffix.is_empty());

        if text_suffix.is_empty() {
            let status = command
                .status()
                .with_context(|| format!("failed to start {}", self.wtype_path.display()))?;
            if !status.success() {
                bail!("wtype replacement failed with status {status}");
            }
            return Ok(());
        }

        command.stdin(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start {}", self.wtype_path.display()))?;
        let mut stdin = child.stdin.take().context("failed to open wtype stdin")?;
        stdin
            .write_all(text_suffix.as_bytes())
            .context("failed to send text to wtype")?;
        drop(stdin);
        let status = child.wait().context("failed to wait for wtype")?;
        if !status.success() {
            bail!("wtype replacement failed with status {status}");
        }
        Ok(())
    }

    fn run_wtype_keys(&self, key: &str, count: usize) -> anyhow::Result<()> {
        if count == 0 {
            return Ok(());
        }

        let mut command = self.wtype_command();
        self.add_wtype_timing_args(&mut command);
        for _ in 0..count {
            command.arg("-k").arg(key);
        }
        let status = command
            .status()
            .with_context(|| format!("failed to start {}", self.wtype_path.display()))?;
        if !status.success() {
            bail!("wtype key injection failed with status {status}");
        }
        Ok(())
    }
}

impl TextInjector for SwayTextInjector {
    fn inject_text(&self, text: &str) -> anyhow::Result<()> {
        self.run_wtype_with_text(text)
            .context("sway/wtype text injection failed")
    }

    fn erase_chars(&self, count: usize) -> anyhow::Result<()> {
        self.run_wtype_keys("BackSpace", count)
            .context("sway/wtype text erasure failed")
    }

    fn replace_tail(&self, erase_count: usize, text_suffix: &str) -> anyhow::Result<()> {
        self.run_wtype_replacement(erase_count, text_suffix)
            .context("sway/wtype text replacement failed")
    }

    fn focused_window(&self) -> anyhow::Result<Option<FocusedWindow>> {
        let output = self
            .swaymsg_command()
            .arg("-r")
            .arg("-t")
            .arg("get_tree")
            .output()
            .context("failed to run swaymsg get_tree")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "swaymsg get_tree failed with status {}: {stderr}",
                output.status
            );
        }

        let tree: Value =
            serde_json::from_slice(&output.stdout).context("failed to parse sway tree JSON")?;
        let focused_id = focused_sway_node_id(&tree).context("failed to find focused Sway node")?;
        Ok(Some(FocusedWindow(focused_id)))
    }
}

fn default_sway_socket_path() -> Option<PathBuf> {
    std::env::var_os("SWAYSOCK")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.exists())
        .or_else(|| {
            let runtime_dir = runtime_dir()?;
            newest_socket_matching(&runtime_dir, |name| {
                name.starts_with("sway-ipc.") && name.ends_with(".sock")
            })
        })
}

fn default_wayland_display() -> Option<String> {
    std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|display| !display.is_empty())
}

fn wayland_display_from_sway_process(sway_socket: &Path) -> Option<String> {
    let pid = sway_pid_from_socket_path(sway_socket)?;
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    environ
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix("WAYLAND_DISPLAY="))
        .filter(|display| !display.is_empty())
        .map(ToOwned::to_owned)
}

fn sway_pid_from_socket_path(sway_socket: &Path) -> Option<u32> {
    let file_name = sway_socket.file_name()?.to_str()?;
    let mut parts = file_name.split('.');
    match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (Some("sway-ipc"), Some(_uid), Some(pid), Some("sock"), None) => pid.parse().ok(),
        _ => None,
    }
}

fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn newest_socket_matching(
    directory: &Path,
    matches_name: impl Fn(&str) -> bool,
) -> Option<PathBuf> {
    std::fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if !matches_name(name) {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            if !metadata.file_type().is_socket() {
                return None;
            }
            let modified = metadata.modified().ok();
            Some((entry.path(), modified))
        })
        .max_by_key(|(_, modified)| *modified)
        .map(|(path, _)| path)
}

fn focused_sway_node_id(value: &Value) -> Option<u64> {
    if value
        .get("focused")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return value.get("id").and_then(Value::as_u64);
    }

    for child_key in ["nodes", "floating_nodes"] {
        let Some(children) = value.get(child_key).and_then(Value::as_array) else {
            continue;
        };
        for child in children {
            if let Some(id) = focused_sway_node_id(child) {
                return Some(id);
            }
        }
    }

    None
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
            bail!("focused desktop target is unavailable; aborting replacement");
        };
        if current_window != target_window {
            self.abort();
            bail!(
                "focused desktop target changed from {} to {}; aborting replacement",
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

pub fn format_transcript_for_injection(transcript: &str, append_space: bool) -> Option<String> {
    let mut text = normalize_transcript_for_injection(transcript)?;
    if append_space {
        text.push(' ');
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

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

    #[test]
    fn focused_sway_node_id_finds_nested_focused_node() {
        let tree = json!({
            "id": 1,
            "focused": false,
            "nodes": [{
                "id": 2,
                "focused": false,
                "nodes": [{
                    "id": 3,
                    "focused": true,
                    "nodes": []
                }],
                "floating_nodes": []
            }],
            "floating_nodes": []
        });

        assert_eq!(focused_sway_node_id(&tree), Some(3));
    }

    #[test]
    fn focused_sway_node_id_checks_floating_nodes() {
        let tree = json!({
            "id": 1,
            "focused": false,
            "nodes": [],
            "floating_nodes": [{
                "id": 4,
                "focused": true
            }]
        });

        assert_eq!(focused_sway_node_id(&tree), Some(4));
    }

    #[test]
    fn focused_sway_node_id_returns_none_without_focus() {
        let tree = json!({
            "id": 1,
            "focused": false,
            "nodes": [{"id": 2, "focused": false}],
            "floating_nodes": []
        });

        assert_eq!(focused_sway_node_id(&tree), None);
    }

    #[test]
    fn sway_pid_from_socket_path_parses_standard_socket_name() {
        assert_eq!(
            sway_pid_from_socket_path(Path::new("/run/user/1001/sway-ipc.1001.77911.sock")),
            Some(77911)
        );
    }

    #[test]
    fn sway_pid_from_socket_path_rejects_other_socket_names() {
        assert_eq!(
            sway_pid_from_socket_path(Path::new("/run/user/1001/wayland-1")),
            None
        );
    }

    #[test]
    fn sway_text_command_uses_delay_as_initial_settle_and_key_delay() {
        let injector = test_sway_injector(20);
        let mut command = injector.wtype_command();

        injector.add_wtype_replacement_args(&mut command, 0, true);

        assert_eq!(command_args(&command), vec!["-s", "20", "-d", "20", "-"]);
    }

    #[test]
    fn sway_text_command_omits_timing_args_without_delay() {
        let injector = test_sway_injector(0);
        let mut command = injector.wtype_command();

        injector.add_wtype_replacement_args(&mut command, 0, true);

        assert_eq!(command_args(&command), vec!["-"]);
    }

    #[test]
    fn sway_replacement_command_combines_backspaces_and_text() {
        let injector = test_sway_injector(20);
        let mut command = injector.wtype_command();

        injector.add_wtype_replacement_args(&mut command, 2, true);

        assert_eq!(
            command_args(&command),
            vec![
                "-s",
                "20",
                "-d",
                "20",
                "-k",
                "BackSpace",
                "-k",
                "BackSpace",
                "-"
            ]
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

    fn test_sway_injector(delay_millis: u32) -> SwayTextInjector {
        SwayTextInjector {
            delay_millis,
            sway_socket: PathBuf::from("/run/user/1001/sway-ipc.1001.42.sock"),
            wayland_display: None,
            swaymsg_path: PathBuf::from("swaymsg"),
            wtype_path: PathBuf::from("wtype"),
        }
    }

    fn command_args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
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
