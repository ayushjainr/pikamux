//! Absolute per-exchange deadlines wrap every TLS socket operation. A slow
//! client cannot extend the foreground capability by dribbling bytes.
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
pub(super) struct DeadlineIo {
    stream: TcpStream,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    interactive: bool,
}
impl DeadlineIo {
    pub(super) fn new(
        stream: TcpStream,
        deadline: Instant,
        cancel: Arc<AtomicBool>,
        interactive: bool,
    ) -> Self {
        Self {
            stream,
            deadline,
            cancel,
            interactive,
        }
    }
    fn check(&self) -> io::Result<Duration> {
        if self.interactive && crate::mobile_pairing_ui::cancelled().map_err(io::Error::other)? {
            self.cancel.store(true, Ordering::Release);
        }
        if self.cancel.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Pairing cancelled",
            ));
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Pairing exchange expired",
            ));
        }
        Ok(remaining.min(Duration::from_millis(100)))
    }
}
impl Read for DeadlineIo {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            self.stream.set_read_timeout(Some(self.check()?))?;
            match self.stream.read(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                result => return result,
            }
        }
    }
}
impl Write for DeadlineIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.stream.set_write_timeout(Some(self.check()?))?;
            match self.stream.write(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.check()?;
        self.stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn silent_peer_cannot_extend_absolute_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let started = Instant::now();
        let mut bounded = DeadlineIo::new(
            stream,
            started + Duration::from_millis(60),
            Arc::new(AtomicBool::new(false)),
            false,
        );
        assert_eq!(
            bounded.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn cancellation_vetoes_socket_io() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut bounded = DeadlineIo::new(
            stream,
            Instant::now() + Duration::from_secs(2),
            Arc::new(AtomicBool::new(true)),
            false,
        );
        assert_eq!(
            bounded.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }
}
