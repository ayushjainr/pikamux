//! Cancel only the exact owned pipe's outstanding I/O. Rust's blocking stdio
//! API uses overlapped Windows pipes internally, requiring CancelIoEx rather
//! than thread-level CancelSynchronousIo.
use crate::consult::CancellationToken;
use std::{
    io,
    os::windows::io::{AsRawHandle, OwnedHandle},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CancelIoEx(file: *mut std::ffi::c_void, overlapped: *mut std::ffi::c_void) -> i32;
}

pub(crate) struct PipeInterrupt {
    active: Arc<Mutex<bool>>,
    finish: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl PipeInterrupt {
    pub(crate) fn new(stop: CancellationToken, handle: OwnedHandle) -> io::Result<Self> {
        let active = Arc::new(Mutex::new(false));
        let watching = active.clone();
        let (finish, finished) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("pika-pipe-cancel".into())
            .spawn(move || {
                loop {
                    match finished.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if stop.is_cancelled() {
                        let active = watching.lock().expect("pipe activity poisoned");
                        if *active {
                            // SAFETY: this owned duplicate pins only this Pika
                            // pipe, never another process or pipe. Null cancels
                            // all outstanding I/O on that pipe in this process.
                            // Repetition covers cancellation before kernel entry;
                            // the activity lock fences return to unrelated work.
                            unsafe {
                                CancelIoEx(handle.as_raw_handle(), std::ptr::null_mut());
                            }
                        }
                    }
                }
            })?;
        Ok(Self {
            active,
            finish,
            worker: Some(worker),
        })
    }

    pub(crate) fn call<T>(&self, operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        *self.active.lock().expect("pipe activity poisoned") = true;
        let result = operation();
        *self.active.lock().expect("pipe activity poisoned") = false;
        result
    }
}

impl Drop for PipeInterrupt {
    fn drop(&mut self) {
        let _ = self.finish.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consult::CancellablePipe;
    use std::{
        io::{Read, Write},
        process::{Command, Stdio},
        time::Instant,
    };

    #[test]
    fn cancellation_interrupts_blocked_read_and_write_without_killing_the_peer() {
        for writing in [false, true] {
            let mut child = Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Start-Sleep -Seconds 10",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let stop = CancellationToken::default();
            let token = stop.clone();
            let input = child.stdin.take().unwrap();
            let output = child.stdout.take().unwrap();
            let (done, receive) = mpsc::channel();
            let worker = thread::spawn(move || {
                if writing {
                    let _ =
                        CancellablePipe::new(input, token)
                            .unwrap()
                            .write_all(&vec![b'x'; 8 * 1024 * 1024]);
                } else {
                    let _ = CancellablePipe::new(output, token)
                        .unwrap()
                        .read(&mut [0; 8]);
                }
                let _ = done.send(());
            });
            thread::sleep(Duration::from_millis(100));
            let started = Instant::now();
            stop.cancel();
            let finished = receive.recv_timeout(Duration::from_secs(3)).is_ok();
            let still_live = child.try_wait().unwrap().is_none();
            let _ = child.kill();
            let _ = child.wait();
            worker.join().unwrap();
            assert!(finished, "blocked pipe was not interrupted");
            assert!(still_live, "cancellation must not kill an unrelated peer");
            assert!(started.elapsed() < Duration::from_secs(4));
        }
    }
}
