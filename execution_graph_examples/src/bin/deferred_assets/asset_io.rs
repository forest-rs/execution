// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A controlled local asset service. Pending means a real socket read, not an artificial delay.
//! Workers and joins belong to the application; the graph owns only each request's cancellation.

use std::{
    error::Error,
    fmt,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
    time::Duration,
};

const MAX_BYTES: u64 = 1024 * 1024;
#[derive(Debug)]
pub(super) enum LoadError {
    Io(std::io::Error),
    TooLarge,
    NotText(std::str::Utf8Error),
    Coordinate { index: usize },
    IncompleteTriples,
    WorkerPanicked,
}
impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "asset transport: {error}"),
            Self::TooLarge => f.write_str("asset exceeds the 1 MiB response limit"),
            Self::NotText(error) => write!(f, "asset coordinates must be UTF-8: {error}"),
            Self::Coordinate { index } => write!(f, "coordinate {index} is not an i64 integer"),
            Self::IncompleteTriples => {
                f.write_str("asset must contain nonempty, complete x/y/z triples")
            }
            Self::WorkerPanicked => f.write_str("asset worker panicked"),
        }
    }
}
impl Error for LoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::NotText(error) => Some(error),
            _ => None,
        }
    }
}
impl From<std::io::Error> for LoadError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Default)]
pub(super) struct Storage {
    // Capacity bytes of decoded coordinate Vec buffers. Excludes metadata, allocator and RSS.
    // A published mesh owns its Reply, so this follows the buffer through queue/task/undo roots.
    pub live_positions: AtomicUsize,
}
pub(super) struct Reply {
    pub coordinates: Vec<i64>,
    bytes: usize,
    storage: Arc<Storage>,
}
impl Drop for Reply {
    fn drop(&mut self) {
        self.storage
            .live_positions
            .fetch_sub(self.bytes, Ordering::Relaxed);
    }
}
struct Mailbox {
    cancelled: bool,
    result: Option<Result<Reply, LoadError>>,
    waker: Option<Waker>,
}

pub(super) struct Request {
    pub id: usize,
    socket: TcpStream,
    mailbox: Arc<Mutex<Mailbox>>,
}
impl Request {
    pub(super) fn poll(&mut self, cx: &Context<'_>) -> Poll<Result<Reply, LoadError>> {
        let mut mailbox = self.mailbox.lock().expect("asset mailbox");
        if let Some(result) = mailbox.result.take() {
            return Poll::Ready(result);
        }
        mailbox.waker = Some(cx.waker().clone());
        Poll::Pending
    }
    pub(super) fn description(&self) -> &'static str {
        if self.mailbox.lock().expect("asset mailbox").result.is_some() {
            "asset.load_points: response queued; guest has not admitted it"
        } else {
            "asset.load_points: waiting for socket response"
        }
    }
}
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        if let Ok(mut mailbox) = self.mailbox.lock() {
            mailbox.cancelled = true;
            mailbox.result.take();
            mailbox.waker.take();
        }
        // Closing our read side unblocks a worker. Joining is deliberately outside graph polling.
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}
struct Worker {
    label: String,
    provider: TcpStream,
    thread: Option<JoinHandle<()>>,
    mailbox: Weak<Mutex<Mailbox>>,
}
pub(super) struct Service {
    workers: Vec<Worker>,
    pub storage: Arc<Storage>,
}
impl Service {
    pub(super) fn new() -> Self {
        Self {
            workers: Vec::new(),
            storage: Arc::new(Storage::default()),
        }
    }
    pub(super) fn start(&mut self, label: &str) -> Result<Request, LoadError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let reader = TcpStream::connect(listener.local_addr()?)?;
        let (provider, _) = listener.accept()?;
        // A failure bound only: successful demonstrations synchronize through reads/joins/wakes.
        reader.set_read_timeout(Some(Duration::from_secs(10)))?;
        let socket = reader.try_clone()?;
        let mailbox = Arc::new(Mutex::new(Mailbox {
            cancelled: false,
            result: None,
            waker: None,
        }));
        let worker_mailbox = Arc::clone(&mailbox);
        let storage = Arc::clone(&self.storage);
        let worker = thread::spawn(move || {
            let result = read_asset(reader, storage);
            let wake = {
                let mut mailbox = worker_mailbox.lock().expect("asset mailbox");
                if mailbox.cancelled {
                    return;
                }
                mailbox.result = Some(result);
                mailbox.waker.take()
            };
            if let Some(waker) = wake {
                waker.wake();
            }
        });
        let id = self.workers.len();
        self.workers.push(Worker {
            label: label.into(),
            provider,
            thread: Some(worker),
            mailbox: Arc::downgrade(&mailbox),
        });
        Ok(Request {
            id,
            socket,
            mailbox,
        })
    }
    pub(super) fn len(&self) -> usize {
        self.workers.len()
    }
    pub(super) fn label(&self, id: usize) -> &str {
        &self.workers[id].label
    }
    pub(super) fn is_retired(&self, id: usize) -> bool {
        self.workers[id]
            .mailbox
            .upgrade()
            .is_none_or(|mailbox| mailbox.lock().expect("asset mailbox").cancelled)
    }
    /// Supply a real response, then wait outside graph execution for the worker to finish.
    /// A queued response is now available, but it has not been admitted or published by the VM.
    pub(super) fn reply(&mut self, id: usize, bytes: &[u8]) -> Result<(), LoadError> {
        let worker = &mut self.workers[id];
        worker.provider.write_all(bytes)?;
        worker.provider.shutdown(Shutdown::Write)?;
        if let Some(thread) = worker.thread.take() {
            thread.join().map_err(|_| LoadError::WorkerPanicked)?;
        }
        Ok(())
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        for worker in &mut self.workers {
            let _ = worker.provider.shutdown(Shutdown::Both);
        }
        for worker in &mut self.workers {
            if let Some(thread) = worker.thread.take() {
                let _ = thread.join();
            }
        }
    }
}
fn read_asset(reader: TcpStream, storage: Arc<Storage>) -> Result<Reply, LoadError> {
    let mut bytes = Vec::new();
    reader.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(LoadError::TooLarge);
    }
    let text = std::str::from_utf8(&bytes).map_err(LoadError::NotText)?;
    let coordinates: Vec<_> = text
        .split_whitespace()
        .enumerate()
        .map(|(index, coordinate)| {
            coordinate
                .parse::<i64>()
                .map_err(|_| LoadError::Coordinate { index })
        })
        .collect::<Result<_, _>>()?;
    if coordinates.is_empty() || coordinates.len() % 3 != 0 {
        return Err(LoadError::IncompleteTriples);
    }
    let bytes = coordinates.capacity() * size_of::<i64>();
    storage.live_positions.fetch_add(bytes, Ordering::Relaxed);
    Ok(Reply {
        coordinates,
        bytes,
        storage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Wake;

    #[derive(Default)]
    struct Wakes(AtomicUsize);
    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    #[test]
    fn cancellation_unblocks_an_unanswered_socket_without_publishing_or_waking() {
        let mut service = Service::new();
        let mut request = service.start("cancel-before-response").unwrap();
        let wakes = Arc::new(Wakes::default());
        let waker = Waker::from(wakes.clone());
        assert!(request.poll(&Context::from_waker(&waker)).is_pending());
        drop(request);
        // Join directly: cancellation must unblock the actual reader, without a provider reply.
        service.workers[0].thread.take().unwrap().join().unwrap();
        assert!(service.is_retired(0));
        assert_eq!(wakes.0.load(Ordering::Relaxed), 0);
        assert_eq!(service.storage.live_positions.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn malformed_text_and_partial_vertices_return_typed_errors_and_no_native_buffer() {
        let mut service = Service::new();
        let waker = Waker::noop();
        let cx = Context::from_waker(waker);
        for (bytes, not_text) in [(&b"\xff"[..], true), (&b"0 1"[..], false)] {
            let mut request = service.start("malformed").unwrap();
            service.reply(request.id, bytes).unwrap();
            let Poll::Ready(Err(error)) = request.poll(&cx) else {
                panic!("joined worker must have queued its typed failure");
            };
            assert!(matches!(error, LoadError::NotText(_)) == not_text);
            if !not_text {
                assert!(matches!(error, LoadError::IncompleteTriples));
            }
            assert_eq!(service.storage.live_positions.load(Ordering::Relaxed), 0);
        }
    }
}
