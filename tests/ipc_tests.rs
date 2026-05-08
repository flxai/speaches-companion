use std::path::Path;

use speaches_companion::daemon::{DaemonResponse, DaemonState};
use speaches_companion::ipc::{
    command_line, default_socket_path, parse_command, send_command, IpcCommand,
};
use tempfile::tempdir;

#[test]
fn ipc_command_lines_match_i3_hotkey_commands() {
    assert_eq!(command_line(IpcCommand::HotkeyDown), "hotkey down");
    assert_eq!(command_line(IpcCommand::HotkeyUp), "hotkey up");
}

#[test]
fn parses_hotkey_commands_from_i3_clients() {
    assert_eq!(
        parse_command("hotkey down\n").unwrap(),
        IpcCommand::HotkeyDown
    );
    assert_eq!(parse_command(" hotkey up ").unwrap(), IpcCommand::HotkeyUp);
    assert!(parse_command("hotkey repeat").is_err());
}

#[test]
fn daemon_state_is_idempotent_for_repeated_down_and_up() {
    let mut state = DaemonState::default();

    assert_eq!(
        state.handle(IpcCommand::HotkeyDown),
        DaemonResponse::Started
    );
    assert!(state.is_recording());
    assert_eq!(
        state.handle(IpcCommand::HotkeyDown),
        DaemonResponse::AlreadyRecording
    );
    assert!(state.is_recording());
    assert_eq!(state.handle(IpcCommand::HotkeyUp), DaemonResponse::Stopped);
    assert!(!state.is_recording());
    assert_eq!(
        state.handle(IpcCommand::HotkeyUp),
        DaemonResponse::AlreadyIdle
    );
}

#[test]
fn response_strings_are_stable_for_i3_clients() {
    assert_eq!(DaemonResponse::Started.as_str(), "started");
    assert_eq!(
        DaemonResponse::AlreadyRecording.as_str(),
        "already-recording"
    );
    assert_eq!(DaemonResponse::Stopped.as_str(), "stopped");
    assert_eq!(DaemonResponse::AlreadyIdle.as_str(), "already-idle");
}

#[test]
fn default_socket_path_uses_runtime_directory_when_available() {
    let path = default_socket_path();

    assert!(path.ends_with(Path::new("speaches-companion.sock")));
    assert!(path.is_absolute());
}

#[tokio::test]
async fn hotkey_client_sends_command_to_unix_socket() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("speaches-companion.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut request)
            .await
            .unwrap();
        assert_eq!(request, "hotkey down\n");
    });

    let response = send_command(&socket_path, IpcCommand::HotkeyDown)
        .await
        .unwrap();
    assert_eq!(response, "");
    server.await.unwrap();
}
