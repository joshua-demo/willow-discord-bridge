use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_IO_PENDING, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Storage::FileSystem::{FILE_FLAG_OVERLAPPED, ReadFile, WriteFile},
        System::{
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
            Pipes::PeekNamedPipe,
            Threading::{CreateEventW, WaitForSingleObject},
        },
    },
    core::{HRESULT, PCWSTR},
};

pub(crate) const RPC_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) trait RpcTransport: Read + Write {
    fn begin_exchange(&mut self, timeout: Duration);
    fn available(&self) -> io::Result<usize>;
}

pub(crate) struct DiscordPipe {
    file: File,
    deadline: Instant,
}

struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

impl DiscordPipe {
    pub(crate) fn open() -> io::Result<Self> {
        let mut last_error = None;
        for index in 0..10 {
            let path = format!(r"\\?\pipe\discord-ipc-{index}");
            match Self::open_path(&path) {
                Ok(pipe) => return Ok(pipe),
                Err(error) if error.kind() != io::ErrorKind::NotFound => last_error = Some(error),
                Err(_) => {}
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Discord desktop RPC pipe was not found",
            )
        }))
    }

    fn open_path(path: &str) -> io::Result<Self> {
        Ok(Self {
            file: OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FILE_FLAG_OVERLAPPED.0)
                .open(path)?,
            deadline: Instant::now() + RPC_TIMEOUT,
        })
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.file.as_raw_handle())
    }

    fn transfer(&mut self, bytes: *mut u8, length: usize, writing: bool) -> io::Result<usize> {
        if length == 0 {
            return Ok(0);
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Discord RPC timed out",
            ));
        }
        let event =
            Event(unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(os_error)?);
        let mut overlapped = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        let mut transferred = 0;
        // The slice and OVERLAPPED remain alive until completion, including after
        // cancellation. Never return while Windows can still access either buffer.
        let started = unsafe {
            if writing {
                WriteFile(
                    self.handle(),
                    Some(std::slice::from_raw_parts(bytes, length)),
                    None,
                    Some(&mut overlapped),
                )
            } else {
                ReadFile(
                    self.handle(),
                    Some(std::slice::from_raw_parts_mut(bytes, length)),
                    None,
                    Some(&mut overlapped),
                )
            }
        };
        if let Err(error) = started {
            if error.code() != HRESULT::from_win32(ERROR_IO_PENDING.0) {
                return Err(os_error(error));
            }
        }
        let timeout = self.deadline.saturating_duration_since(Instant::now());
        let milliseconds = timeout
            .as_millis()
            .saturating_add(1)
            .min(u32::MAX as u128 - 1) as u32;
        let waited = unsafe { WaitForSingleObject(event.0, milliseconds) };
        if waited != WAIT_OBJECT_0 {
            let error = if waited == WAIT_TIMEOUT {
                io::Error::new(io::ErrorKind::TimedOut, "Discord RPC timed out")
            } else {
                io::Error::last_os_error()
            };
            unsafe {
                let _ = CancelIoEx(self.handle(), Some(&overlapped));
                // Drain cancelled I/O before dropping its OVERLAPPED/buffer.
                let _ = GetOverlappedResult(self.handle(), &overlapped, &mut transferred, true);
            }
            return Err(error);
        }
        unsafe { GetOverlappedResult(self.handle(), &overlapped, &mut transferred, false) }
            .map_err(os_error)?;
        Ok(transferred as usize)
    }
}

fn os_error(error: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error((error.code().0 as u32 & 0xffff) as i32)
}

impl RpcTransport for DiscordPipe {
    fn begin_exchange(&mut self, timeout: Duration) {
        self.deadline = Instant::now() + timeout;
    }

    fn available(&self) -> io::Result<usize> {
        let mut bytes = 0;
        unsafe { PeekNamedPipe(self.handle(), None, 0, None, Some(&mut bytes), None) }
            .map_err(os_error)?;
        Ok(bytes as usize)
    }
}

impl Read for DiscordPipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.transfer(bytes.as_mut_ptr(), bytes.len(), false)
    }
}

impl Write for DiscordPipe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.transfer(bytes.as_ptr() as *mut u8, bytes.len(), true)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "pipe_tests.rs"]
mod tests;

#[cfg(test)]
impl RpcTransport for File {
    fn begin_exchange(&mut self, _: Duration) {}
    fn available(&self) -> io::Result<usize> {
        Ok(0)
    }
}
