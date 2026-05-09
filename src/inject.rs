use std::ffi::CString;
use std::io::Write;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::Value;

pub const DEFAULT_PASTE_SETTLE_DELAY_MS: u64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusedWindow(pub u64);

pub trait TextInjector: Send + Sync {
    fn inject_text(&self, text: &str) -> anyhow::Result<()>;

    fn should_show_listening_marker(&self) -> anyhow::Result<bool> {
        Ok(true)
    }

    fn press_enter(&self) -> anyhow::Result<()> {
        self.inject_text("\n")
    }

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
    paste_settle_delay_ms: u64,
    paste_in_terminals: bool,
}

impl DesktopTextInjector {
    pub fn new(delay_microsecs: u32) -> Self {
        Self::new_with_options(delay_microsecs, DEFAULT_PASTE_SETTLE_DELAY_MS, false)
    }

    pub fn new_with_paste_settle_delay(delay_microsecs: u32, paste_settle_delay_ms: u64) -> Self {
        Self::new_with_options(delay_microsecs, paste_settle_delay_ms, false)
    }

    pub fn new_with_options(
        delay_microsecs: u32,
        paste_settle_delay_ms: u64,
        paste_in_terminals: bool,
    ) -> Self {
        Self {
            delay_microsecs,
            paste_settle_delay_ms,
            paste_in_terminals,
        }
    }

    fn backend(&self) -> DesktopTextBackend {
        if let Some(injector) = SwayTextInjector::detect(
            self.delay_microsecs,
            self.paste_settle_delay_ms,
            self.paste_in_terminals,
        ) {
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

    fn should_show_listening_marker(&self) -> anyhow::Result<bool> {
        match self {
            Self::Sway(injector) => injector.should_show_listening_marker(),
            Self::X11(injector) => injector.should_show_listening_marker(),
        }
    }

    fn press_enter(&self) -> anyhow::Result<()> {
        match self {
            Self::Sway(injector) => injector.press_enter(),
            Self::X11(injector) => injector.press_enter(),
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

    fn should_show_listening_marker(&self) -> anyhow::Result<bool> {
        self.backend().should_show_listening_marker()
    }

    fn press_enter(&self) -> anyhow::Result<()> {
        self.backend().press_enter()
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

    fn press_enter(&self) -> anyhow::Result<()> {
        let xdo = RawXdo::new()?;
        xdo.with_cleared_modifiers(|| xdo.send_keysequence("Return", self.delay_microsecs))
            .context("libxdo enter key injection failed")
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
    paste_settle_delay_ms: u64,
    paste_in_terminals: bool,
    sway_socket: PathBuf,
    wayland_display: Option<String>,
    swaymsg_path: PathBuf,
    wtype_path: PathBuf,
    wl_copy_path: PathBuf,
    wl_paste_path: PathBuf,
}

impl SwayTextInjector {
    pub fn detect(
        delay_microsecs: u32,
        paste_settle_delay_ms: u64,
        paste_in_terminals: bool,
    ) -> Option<Self> {
        let sway_socket = default_sway_socket_path()?;
        let wayland_display =
            default_wayland_display().or_else(|| wayland_display_from_sway_process(&sway_socket));
        Some(Self {
            delay_millis: delay_microsecs.div_ceil(1000),
            paste_settle_delay_ms,
            paste_in_terminals,
            sway_socket,
            wayland_display,
            swaymsg_path: PathBuf::from("swaymsg"),
            wtype_path: PathBuf::from("wtype"),
            wl_copy_path: PathBuf::from("wl-copy"),
            wl_paste_path: PathBuf::from("wl-paste"),
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

    fn wl_copy_command(&self) -> Command {
        self.wayland_command(&self.wl_copy_path)
    }

    fn wl_paste_command(&self) -> Command {
        self.wayland_command(&self.wl_paste_path)
    }

    fn wayland_command(&self, path: &Path) -> Command {
        let mut command = Command::new(path);
        if let Some(wayland_display) = self.wayland_display.as_ref() {
            command.env("WAYLAND_DISPLAY", wayland_display);
        }
        command
    }

    fn add_wtype_timing_args(&self, command: &mut Command, delay_millis: u32) {
        if delay_millis > 0 {
            let delay = delay_millis.to_string();
            command.arg("-s").arg(&delay);
            command.arg("-d").arg(delay);
        }
    }

    fn add_wtype_replacement_args(
        &self,
        command: &mut Command,
        erase_count: usize,
        has_text_suffix: bool,
        delay_millis: u32,
    ) {
        self.add_wtype_timing_args(command, delay_millis);
        for _ in 0..erase_count {
            command.arg("-k").arg("BackSpace");
        }
        if has_text_suffix {
            command.arg("-");
        }
    }

    fn add_wtype_paste_args(
        &self,
        command: &mut Command,
        erase_count: usize,
        paste_mode: PasteMode,
    ) {
        self.add_wtype_timing_args(command, self.delay_millis);
        for _ in 0..erase_count {
            command.arg("-k").arg("BackSpace");
        }
        if self.delay_millis > 0 && erase_count > 0 {
            command.arg("-s").arg(self.delay_millis.to_string());
        }
        command.arg("-M").arg("ctrl");
        if matches!(paste_mode, PasteMode::Terminal) {
            command.arg("-M").arg("shift");
        }
        command.arg("-P").arg("v").arg("-p").arg("v");
        if matches!(paste_mode, PasteMode::Terminal) {
            command.arg("-m").arg("shift");
        }
        command.arg("-m").arg("ctrl");
    }

    fn run_wtype_with_text(&self, text: &str) -> anyhow::Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        self.run_hybrid_replacement(0, text)
    }

    fn run_hybrid_replacement(&self, erase_count: usize, text_suffix: &str) -> anyhow::Result<()> {
        let paste_mode = self.paste_mode_for_suffix(text_suffix)?;
        if let Some(paste_mode) = paste_mode {
            match self.try_clipboard_replacement(erase_count, text_suffix, paste_mode) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => {
                    eprintln!(
                        "speaches-companion clipboard text injection failed; falling back to wtype: {error:#}"
                    );
                }
            }
        }

        let typed_delay_millis = if paste_mode.is_some() {
            0
        } else {
            self.delay_millis
        };
        self.run_wtype_typed_replacement(erase_count, text_suffix, typed_delay_millis)
    }

    fn run_wtype_typed_replacement(
        &self,
        erase_count: usize,
        text_suffix: &str,
        delay_millis: u32,
    ) -> anyhow::Result<()> {
        if erase_count == 0 && text_suffix.is_empty() {
            return Ok(());
        }

        let mut command = self.wtype_command();
        self.add_wtype_replacement_args(
            &mut command,
            erase_count,
            !text_suffix.is_empty(),
            delay_millis,
        );

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

    fn try_clipboard_replacement(
        &self,
        erase_count: usize,
        text_suffix: &str,
        paste_mode: PasteMode,
    ) -> anyhow::Result<bool> {
        let snapshot = self.clipboard_snapshot()?;
        if matches!(snapshot, ClipboardSnapshot::NonText) {
            return Ok(false);
        };

        self.copy_text_to_clipboard(text_suffix.as_bytes())?;
        let paste_result = self.run_wtype_paste(erase_count, paste_mode);
        self.wait_for_paste_delivery();
        if let Err(error) = self.restore_clipboard(snapshot) {
            eprintln!("speaches-companion failed to restore clipboard: {error:#}");
        }
        paste_result?;
        Ok(true)
    }

    fn paste_mode_for_suffix(&self, text_suffix: &str) -> anyhow::Result<Option<PasteMode>> {
        if text_suffix.is_empty() {
            return Ok(None);
        }

        if self.focused_target_is_terminal()? {
            Ok(self.paste_in_terminals.then_some(PasteMode::Terminal))
        } else {
            Ok(Some(PasteMode::Gui))
        }
    }

    fn focused_target_is_terminal(&self) -> anyhow::Result<bool> {
        let tree = self.sway_tree()?;
        let Some(node) = focused_sway_node(&tree) else {
            return Ok(false);
        };
        Ok(sway_node_identity(node).is_some_and(is_terminal_identity))
    }

    fn sway_tree(&self) -> anyhow::Result<Value> {
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

        serde_json::from_slice(&output.stdout).context("failed to parse sway tree JSON")
    }

    fn run_wtype_paste(&self, erase_count: usize, paste_mode: PasteMode) -> anyhow::Result<()> {
        let mut command = self.wtype_command();
        self.add_wtype_paste_args(&mut command, erase_count, paste_mode);
        let status = command
            .status()
            .with_context(|| format!("failed to start {}", self.wtype_path.display()))?;
        if !status.success() {
            bail!("wtype paste injection failed with status {status}");
        }
        Ok(())
    }

    fn clipboard_snapshot(&self) -> anyhow::Result<ClipboardSnapshot> {
        let output = self
            .wl_paste_command()
            .args(["--list-types"])
            .output()
            .with_context(|| format!("failed to start {}", self.wl_paste_path.display()))?;
        if !output.status.success() {
            return Ok(ClipboardSnapshot::Empty);
        }

        let types = String::from_utf8_lossy(&output.stdout);
        let mut has_type = false;
        let mut has_text = false;
        for mime_type in types.lines() {
            has_type = true;
            has_text |= is_text_mime_type(mime_type);
        }
        if !has_type {
            return Ok(ClipboardSnapshot::Empty);
        }
        if !has_text {
            return Ok(ClipboardSnapshot::NonText);
        }

        let output = self
            .wl_paste_command()
            .args(["--no-newline", "--type", "text"])
            .output()
            .with_context(|| format!("failed to start {}", self.wl_paste_path.display()))?;
        if !output.status.success() {
            return Ok(ClipboardSnapshot::Empty);
        }
        Ok(ClipboardSnapshot::Text(output.stdout))
    }

    fn copy_text_to_clipboard(&self, text: &[u8]) -> anyhow::Result<()> {
        let mut last_error = None;
        for attempt in 0..3 {
            match self.copy_text_to_clipboard_once(text) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    last_error = Some(error);
                    if attempt < 2 {
                        thread::sleep(Duration::from_millis(25 * (attempt + 1) as u64));
                    }
                }
            }
        }

        Err(last_error.expect("copy attempts should record last error"))
    }

    fn copy_text_to_clipboard_once(&self, text: &[u8]) -> anyhow::Result<()> {
        let mut command = self.wl_copy_command();
        command
            .args(["--type", "text/plain"])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start {}", self.wl_copy_path.display()))?;
        let mut stdin = child.stdin.take().context("failed to open wl-copy stdin")?;
        stdin
            .write_all(text)
            .context("failed to send text to wl-copy")?;
        drop(stdin);
        let output = child
            .wait_with_output()
            .context("failed to wait for wl-copy")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("wl-copy failed with status {}: {stderr}", output.status);
        }
        Ok(())
    }

    fn restore_clipboard(&self, snapshot: ClipboardSnapshot) -> anyhow::Result<()> {
        match snapshot {
            ClipboardSnapshot::Empty => {
                let status = self
                    .wl_copy_command()
                    .arg("--clear")
                    .status()
                    .with_context(|| format!("failed to start {}", self.wl_copy_path.display()))?;
                if !status.success() {
                    bail!("wl-copy --clear failed with status {status}");
                }
            }
            ClipboardSnapshot::Text(text) => self.copy_text_to_clipboard(&text)?,
            ClipboardSnapshot::NonText => {}
        }
        Ok(())
    }

    fn wait_for_paste_delivery(&self) {
        let delay = self.paste_settle_delay_ms.max(u64::from(self.delay_millis));
        thread::sleep(Duration::from_millis(delay));
    }

    fn run_wtype_keys(&self, key: &str, count: usize) -> anyhow::Result<()> {
        if count == 0 {
            return Ok(());
        }

        let mut command = self.wtype_command();
        self.add_wtype_timing_args(&mut command, self.delay_millis);
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

    fn should_show_listening_marker(&self) -> anyhow::Result<bool> {
        if self.paste_in_terminals && self.focused_target_is_terminal()? {
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn press_enter(&self) -> anyhow::Result<()> {
        self.run_wtype_keys("Return", 1)
            .context("sway/wtype enter key injection failed")
    }

    fn erase_chars(&self, count: usize) -> anyhow::Result<()> {
        self.run_wtype_keys("BackSpace", count)
            .context("sway/wtype text erasure failed")
    }

    fn replace_tail(&self, erase_count: usize, text_suffix: &str) -> anyhow::Result<()> {
        self.run_hybrid_replacement(erase_count, text_suffix)
            .context("sway text replacement failed")
    }

    fn focused_window(&self) -> anyhow::Result<Option<FocusedWindow>> {
        let tree = self.sway_tree()?;
        let focused_id = focused_sway_node_id(&tree).context("failed to find focused Sway node")?;
        Ok(Some(FocusedWindow(focused_id)))
    }
}

enum ClipboardSnapshot {
    Empty,
    Text(Vec<u8>),
    NonText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasteMode {
    Gui,
    Terminal,
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
    focused_sway_node(value)?.get("id").and_then(Value::as_u64)
}

fn focused_sway_node(value: &Value) -> Option<&Value> {
    if value
        .get("focused")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Some(value);
    }

    for child_key in ["nodes", "floating_nodes"] {
        let Some(children) = value.get(child_key).and_then(Value::as_array) else {
            continue;
        };
        for child in children {
            if let Some(node) = focused_sway_node(child) {
                return Some(node);
            }
        }
    }

    None
}

fn sway_node_identity(value: &Value) -> Option<String> {
    let app_id = value.get("app_id").and_then(Value::as_str).unwrap_or("");
    let class = value
        .get("window_properties")
        .and_then(|properties| properties.get("class"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = value.get("name").and_then(Value::as_str).unwrap_or("");
    let identity = [app_id, class, name]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\t");
    if identity.is_empty() {
        None
    } else {
        Some(identity)
    }
}

fn is_terminal_identity(identity: String) -> bool {
    let identity = identity.to_ascii_lowercase();
    [
        "alacritty",
        "foot",
        "kitty",
        "wezterm",
        "wezfurlong",
        "gnome-terminal",
        "kgx",
        "konsole",
        "xterm",
    ]
    .into_iter()
    .any(|terminal| identity.contains(terminal))
}

fn is_text_mime_type(mime_type: &str) -> bool {
    let mime_type = mime_type.trim().to_ascii_lowercase();
    mime_type == "utf8_string"
        || mime_type == "string"
        || mime_type == "text"
        || mime_type.starts_with("text/")
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

        injector.add_wtype_replacement_args(&mut command, 0, true, 20);

        assert_eq!(command_args(&command), vec!["-s", "20", "-d", "20", "-"]);
    }

    #[test]
    fn sway_text_command_omits_timing_args_without_delay() {
        let injector = test_sway_injector(0);
        let mut command = injector.wtype_command();

        injector.add_wtype_replacement_args(&mut command, 0, true, 0);

        assert_eq!(command_args(&command), vec!["-"]);
    }

    #[test]
    fn sway_replacement_command_combines_backspaces_and_text() {
        let injector = test_sway_injector(20);
        let mut command = injector.wtype_command();

        injector.add_wtype_replacement_args(&mut command, 2, true, 20);

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

    #[test]
    fn sway_gui_fallback_command_omits_delay() {
        let injector = test_sway_injector(20);
        let mut command = injector.wtype_command();

        injector.add_wtype_replacement_args(&mut command, 2, true, 0);

        assert_eq!(
            command_args(&command),
            vec!["-k", "BackSpace", "-k", "BackSpace", "-"]
        );
    }

    #[test]
    fn sway_paste_command_combines_backspaces_and_paste_shortcut() {
        let injector = test_sway_injector(20);
        let mut command = injector.wtype_command();

        injector.add_wtype_paste_args(&mut command, 2, PasteMode::Gui);

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
                "-s",
                "20",
                "-M",
                "ctrl",
                "-P",
                "v",
                "-p",
                "v",
                "-m",
                "ctrl"
            ]
        );
    }

    #[test]
    fn sway_terminal_paste_command_uses_ctrl_shift_v() {
        let injector = test_sway_injector(20);
        let mut command = injector.wtype_command();

        injector.add_wtype_paste_args(&mut command, 2, PasteMode::Terminal);

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
                "-s",
                "20",
                "-M",
                "ctrl",
                "-M",
                "shift",
                "-P",
                "v",
                "-p",
                "v",
                "-m",
                "shift",
                "-m",
                "ctrl"
            ]
        );
    }

    #[test]
    fn sway_terminal_identity_matches_common_terminal_nodes() {
        assert!(is_terminal_identity("Alacritty".to_string()));
        assert!(is_terminal_identity("org.wezfurlong.wezterm".to_string()));
        assert!(!is_terminal_identity("firefox\tBrowser".to_string()));
    }

    #[test]
    fn text_mime_type_matches_wayland_text_offers() {
        assert!(is_text_mime_type("text/plain;charset=utf-8"));
        assert!(is_text_mime_type("UTF8_STRING"));
        assert!(!is_text_mime_type("image/png"));
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
            paste_settle_delay_ms: DEFAULT_PASTE_SETTLE_DELAY_MS,
            paste_in_terminals: false,
            sway_socket: PathBuf::from("/run/user/1001/sway-ipc.1001.42.sock"),
            wayland_display: None,
            swaymsg_path: PathBuf::from("swaymsg"),
            wtype_path: PathBuf::from("wtype"),
            wl_copy_path: PathBuf::from("wl-copy"),
            wl_paste_path: PathBuf::from("wl-paste"),
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
