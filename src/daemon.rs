use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context};
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::Mutex;

use crate::ipc::parse_command;
use crate::ipc::IpcCommand;

#[derive(Debug, Default)]
pub struct DaemonState {
    recording: bool,
}

#[async_trait]
pub trait HotkeyHandler: Send {
    async fn handle_hotkey(&mut self, command: IpcCommand) -> anyhow::Result<DaemonResponse>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonResponse {
    Started,
    AlreadyRecording,
    Stopped,
    AlreadyIdle,
}

impl DaemonResponse {
    pub fn as_str(self) -> &'static str {
        match self {
            DaemonResponse::Started => "started",
            DaemonResponse::AlreadyRecording => "already-recording",
            DaemonResponse::Stopped => "stopped",
            DaemonResponse::AlreadyIdle => "already-idle",
        }
    }
}

impl DaemonState {
    pub fn is_recording(&self) -> bool {
        self.recording
    }

    pub fn handle(&mut self, command: IpcCommand) -> DaemonResponse {
        match (self.recording, command) {
            (false, IpcCommand::HotkeyDown) => {
                self.recording = true;
                DaemonResponse::Started
            }
            (true, IpcCommand::HotkeyDown) => DaemonResponse::AlreadyRecording,
            (true, IpcCommand::HotkeyUp) => {
                self.recording = false;
                DaemonResponse::Stopped
            }
            (false, IpcCommand::HotkeyUp) => DaemonResponse::AlreadyIdle,
        }
    }
}

pub async fn run_daemon<H>(socket_path: &Path, handler: H) -> anyhow::Result<()>
where
    H: HotkeyHandler + 'static,
{
    prepare_socket_path(socket_path).await?;
    let listener = UnixListener::bind(socket_path).with_context(|| {
        format!(
            "failed to bind speaches-scribe daemon socket {}",
            socket_path.display()
        )
    })?;
    let handler = Arc::new(Mutex::new(handler));

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept speaches-scribe IPC client")?;
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            if let Err(error) = handle_client(stream, handler).await {
                eprintln!("speaches-scribe daemon client error: {error:#}");
            }
        });
    }
}

async fn prepare_socket_path(socket_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    match tokio::fs::metadata(socket_path).await {
        Ok(metadata) if metadata.file_type().is_socket() => {
            tokio::fs::remove_file(socket_path).await.with_context(|| {
                format!("failed to remove stale socket {}", socket_path.display())
            })?;
        }
        Ok(_) => bail!(
            "refusing to replace non-socket path {}",
            socket_path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", socket_path.display()))
        }
    }

    Ok(())
}

async fn handle_client(
    mut stream: tokio::net::UnixStream,
    handler: Arc<Mutex<impl HotkeyHandler + 'static>>,
) -> anyhow::Result<()> {
    let mut request = String::new();
    stream
        .read_to_string(&mut request)
        .await
        .context("failed to read speaches-scribe IPC request")?;
    let command = parse_command(&request)?;
    let response = {
        let mut handler = handler.lock().await;
        handler.handle_hotkey(command).await?
    };
    stream
        .write_all(format!("{}\n", response.as_str()).as_bytes())
        .await
        .context("failed to write speaches-scribe IPC response")?;
    Ok(())
}
