use super::*;
use std::{os::windows::io::FromRawHandle, thread};
use windows::{
    Win32::{
        Foundation::{ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE},
        Storage::FileSystem::PIPE_ACCESS_DUPLEX,
        System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        },
    },
    core::HSTRING,
};

fn test_pipe(partial_header: bool) -> (DiscordPipe, thread::JoinHandle<()>) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = format!(
        r"\\.\pipe\bridge-timeout-test-{}-{unique}",
        std::process::id()
    );
    let name = HSTRING::from(&path);
    let handle = unsafe {
        CreateNamedPipeW(
            &name,
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            1024,
            1024,
            0,
            None,
        )
    };
    assert_ne!(
        handle,
        INVALID_HANDLE_VALUE,
        "{}",
        io::Error::last_os_error()
    );
    let mut server = unsafe { File::from_raw_handle(handle.0) };
    let thread = thread::spawn(move || {
        let connected = unsafe { ConnectNamedPipe(HANDLE(server.as_raw_handle()), None) };
        if let Err(error) = connected {
            assert_eq!(error.code(), HRESULT::from_win32(ERROR_PIPE_CONNECTED.0));
        }
        if partial_header {
            server.write_all(b"1234").unwrap();
        }
        // Keep the server alive without completing client I/O. These tests use
        // an isolated pipe, never Discord's real pipe or its voice state.
        thread::sleep(Duration::from_millis(500));
    });
    (DiscordPipe::open_path(&path).unwrap(), thread)
}

#[test]
fn stalled_native_read_is_cancelled_and_returns_a_timeout() {
    let (mut pipe, server) = test_pipe(false);
    pipe.begin_exchange(Duration::from_millis(50));
    let start = Instant::now();
    let error = pipe.read(&mut [0; 8]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(pipe);
    server.join().unwrap();
}

#[test]
fn incomplete_header_uses_one_deadline_instead_of_hanging() {
    let (mut pipe, server) = test_pipe(true);
    pipe.begin_exchange(Duration::from_millis(50));
    let error = pipe.read_exact(&mut [0; 8]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    drop(pipe);
    server.join().unwrap();
}

#[test]
fn blocked_native_write_is_cancelled_without_dangling_buffers() {
    let (mut pipe, server) = test_pipe(false);
    pipe.begin_exchange(Duration::from_millis(50));
    let bytes = vec![0; 8 * 1024 * 1024];
    let error = pipe.write_all(&bytes).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    drop(pipe);
    drop(bytes);
    server.join().unwrap();
}

#[test]
fn idle_peek_detects_a_closed_native_pipe_without_blocking() {
    let (pipe, server) = test_pipe(false);
    assert_eq!(pipe.available().unwrap(), 0);
    server.join().unwrap();
    assert!(pipe.available().is_err());
}
