use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::Mutex;

use crate::ipc::parse_command;
use crate::ipc::IpcCommand;

#[derive(Debug, Default)]
pub struct DaemonState {
    recording: bool,
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

pub async fn run_daemon(socket_path: &Path) -> anyhow::Result<()> {
    prepare_socket_path(socket_path).await?;
    let listener = UnixListener::bind(socket_path).with_context(|| {
        format!(
            "failed to bind trec daemon socket {}",
            socket_path.display()
        )
    })?;
    let state = Arc::new(Mutex::new(DaemonState::default()));

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept trec IPC client")?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(error) = handle_client(stream, state).await {
                eprintln!("trec daemon client error: {error:#}");
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
    state: Arc<Mutex<DaemonState>>,
) -> anyhow::Result<()> {
    let mut request = String::new();
    stream
        .read_to_string(&mut request)
        .await
        .context("failed to read trec IPC request")?;
    let command = parse_command(&request)?;
    let response = {
        let mut state = state.lock().await;
        state.handle(command)
    };
    stream
        .write_all(format!("{}\n", response.as_str()).as_bytes())
        .await
        .context("failed to write trec IPC response")?;
    Ok(())
}
