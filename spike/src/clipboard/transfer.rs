//! Size and deadline bounds are shared by native pipes and helper processes.
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub const TIMEOUT: Duration = Duration::from_secs(2);

pub fn nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: the descriptor remains live for both fcntl calls.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub fn pipe() -> io::Result<(std::fs::File, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: pipe2 initializes both slots; each returned FD is adopted once.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    nonblocking(&reader)?;
    Ok((reader, writer))
}
fn ready(fd: &impl AsRawFd, events: i16, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Clipboard transfer timed out",
            ));
        }
        let mut p = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: p is a live pollfd, and poll does not retain its address.
        let result = unsafe { libc::poll(&mut p, 1, remaining.as_millis().max(1) as i32) };
        if result > 0 {
            return Ok(());
        }
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
}
pub fn read(mut fd: impl Read + AsRawFd, deadline: Instant) -> io::Result<String> {
    nonblocking(&fd)?;
    let mut data = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        ready(&fd, libc::POLLIN, deadline)?;
        match fd.read(&mut chunk) {
            Ok(0) => return Ok(String::from_utf8_lossy(&data).into_owned()),
            Ok(n) if data.len() + n > MAX_BYTES => {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "Clipboard exceeds 16 MiB",
                ));
            }
            Ok(n) => data.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(e) => return Err(e),
        }
    }
}
pub fn write(mut fd: impl Write + AsRawFd, mut data: &[u8], deadline: Instant) -> io::Result<()> {
    nonblocking(&fd)?;
    while !data.is_empty() {
        ready(&fd, libc::POLLOUT, deadline)?;
        match fd.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => data = &data[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stalled_owner_has_a_deadline() {
        let (reader, _writer) = pipe().unwrap();
        let start = Instant::now();
        let e = read(reader, start + Duration::from_millis(30)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn unicode_and_line_endings_survive() {
        let (reader, writer) = pipe().unwrap();
        let text = "žluťoučký 🦀\r\nlast\r\n";
        write(
            std::fs::File::from(writer),
            text.as_bytes(),
            Instant::now() + TIMEOUT,
        )
        .unwrap();
        assert_eq!(read(reader, Instant::now() + TIMEOUT).unwrap(), text);
    }
    #[test]
    fn large_text_is_not_truncated_at_pipe_capacity() {
        let (reader, writer) = pipe().unwrap();
        let text = "αβ\n".repeat(100_000);
        let expected = text.clone();
        let writer = std::thread::spawn(move || {
            write(
                std::fs::File::from(writer),
                text.as_bytes(),
                Instant::now() + TIMEOUT,
            )
            .unwrap()
        });
        assert_eq!(read(reader, Instant::now() + TIMEOUT).unwrap(), expected);
        writer.join().unwrap();
    }
}
