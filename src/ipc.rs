use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcCommand {
    HotkeyDown,
    HotkeyUp,
}

pub fn parse_command(line: &str) -> anyhow::Result<IpcCommand> {
    match line.trim() {
        "hotkey down" => Ok(IpcCommand::HotkeyDown),
        "hotkey up" => Ok(IpcCommand::HotkeyUp),
        command => bail!("unknown trec IPC command: {command}"),
    }
}

pub fn command_line(command: IpcCommand) -> &'static str {
    match command {
        IpcCommand::HotkeyDown => "hotkey down",
        IpcCommand::HotkeyUp => "hotkey up",
    }
}

pub fn default_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("trec.sock")
}

pub async fn send_command(socket_path: &Path, command: IpcCommand) -> anyhow::Result<String> {
    let mut stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "failed to connect to trec daemon at {}",
            socket_path.display()
        )
    })?;
    stream
        .write_all(format!("{}\n", command_line(command)).as_bytes())
        .await
        .context("failed to send command to trec daemon")?;
    stream
        .shutdown()
        .await
        .context("failed to finish trec daemon request")?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .context("failed to read trec daemon response")?;
    Ok(response.trim().to_string())
}
