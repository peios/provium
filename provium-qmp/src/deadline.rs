//! One elapsed-time budget for socket connection, negotiation, and command I/O.

use std::io::{self, BufRead, BufReader, Write};
use std::mem::{offset_of, size_of};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::QmpError;

pub(super) struct Budget {
    started: Instant,
    timeout: Duration,
}

impl Budget {
    pub(super) fn new(timeout: Duration) -> Result<Self, QmpError> {
        let started = Instant::now();
        if started.checked_add(timeout).is_none() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "QMP timeout is too large").into(),
            );
        }
        Ok(Self { started, timeout })
    }

    pub(super) fn remaining(&self) -> Result<Duration, QmpError> {
        let remaining = self.timeout.saturating_sub(self.started.elapsed());
        if remaining.is_zero() {
            Err(self.expired())
        } else {
            Ok(remaining)
        }
    }

    pub(super) fn expired(&self) -> QmpError {
        QmpError::Timeout(self.timeout)
    }

    fn io_error(&self, error: io::Error) -> QmpError {
        match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => self.expired(),
            _ => QmpError::Io(error),
        }
    }
}

pub(super) fn connect(path: &Path, budget: &Budget) -> Result<UnixStream, QmpError> {
    let (address, length) = socket_address(path)?;
    loop {
        budget.remaining()?;
        // SAFETY: socket creates a new descriptor with no borrowed pointers.
        let raw = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: this successful socket call transfers its only owning descriptor here.
        let socket = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: address is initialized and length covers its family and terminated path.
        let result = unsafe {
            libc::connect(
                socket.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                length,
            )
        };
        if result == 0 {
            let stream = UnixStream::from(socket);
            stream.set_nonblocking(false)?;
            return Ok(stream);
        }
        let error = io::Error::last_os_error();
        // Unix sockets report a full listen queue as EAGAIN, not an in-progress
        // connection. Retry with a fresh descriptor and retain the same budget.
        if matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EINTR)) {
            drop(socket);
            std::thread::sleep(budget.remaining()?.min(Duration::from_millis(2)));
            continue;
        }
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error.into());
        }
        wait_connected(&socket, budget)?;
        let stream = UnixStream::from(socket);
        stream.set_nonblocking(false)?;
        return Ok(stream);
    }
}

fn socket_address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: an all-zero sockaddr_un is a valid initial storage representation.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid QMP socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let length = offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    Ok((address, length as libc::socklen_t))
}

fn wait_connected(socket: &OwnedFd, budget: &Budget) -> Result<(), QmpError> {
    loop {
        let milliseconds = budget
            .remaining()?
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128);
        let mut descriptor = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: descriptor is writable and socket stays owned throughout poll.
        let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds as i32) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if result == 0 {
            continue;
        }
        let mut error: libc::c_int = 0;
        let mut size = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: SO_ERROR writes one int into aligned storage, with its size supplied.
        let result = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut error as *mut libc::c_int).cast(),
                &mut size,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error).into());
        }
        budget.remaining()?;
        return Ok(());
    }
}

pub(super) fn read_line(
    reader: &mut BufReader<UnixStream>,
    budget: &Budget,
) -> Result<String, QmpError> {
    let mut line = Vec::new();
    loop {
        reader
            .get_ref()
            .set_read_timeout(Some(budget.remaining()?))?;
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(budget.io_error(error)),
        };
        if available.is_empty() {
            return Err(QmpError::Closed("EOF during QMP handshake"));
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(available.len(), |index| index + 1);
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            budget.remaining()?;
            return String::from_utf8(line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error).into());
        }
    }
}

pub(super) fn send(
    stream: &mut UnixStream,
    value: serde_json::Value,
    budget: &Budget,
) -> Result<(), QmpError> {
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    let mut remaining = bytes.as_slice();
    while !remaining.is_empty() {
        stream.set_write_timeout(Some(budget.remaining()?))?;
        match stream.write(remaining) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
            Ok(count) => remaining = &remaining[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(budget.io_error(error)),
        }
    }
    budget.remaining()?;
    Ok(())
}
