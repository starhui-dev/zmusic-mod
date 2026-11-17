use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use tokio::runtime::Builder;
use tokio::sync::Notify;
use tokio::time::timeout;

use crate::state::PlayerError;

pub const MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const INITIAL_BUFFER_BYTES: usize = 128 * 1024;
const PLAYBACK_START_BUFFER_BYTES: usize = 256 * 1024;
const REBUFFER_BYTES: usize = 256 * 1024;
const REWIND_BYTES: u64 = 256 * 1024;
const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(2);
const NETWORK_STALL_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const INITIAL_BUFFER_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RECONNECT_ATTEMPTS: usize = 3;
const RECONNECT_BACKOFF: Duration = Duration::from_millis(100);
static TLS_PROVIDER_READY: OnceLock<bool> = OnceLock::new();

pub struct HttpStream {
    shared: Arc<Shared>,
    cursor: u64,
    worker: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct HttpStreamControl {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<BufferState>,
    changed: Condvar,
    cancelled: AtomicBool,
    cancel_notify: Notify,
}

struct BufferState {
    data: VecDeque<u8>,
    base_offset: u64,
    end_offset: u64,
    reader_offset: u64,
    total_size: Option<u64>,
    complete: bool,
    error: Option<PlayerError>,
    requested_seek: Option<u64>,
    resume_threshold: usize,
}

impl HttpStream {
    pub fn start(url: &str) -> Result<Self, PlayerError> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(PlayerError::InvalidArgument);
        }

        let shared = Arc::new(Shared {
            state: Mutex::new(BufferState {
                data: VecDeque::with_capacity(MAX_BUFFER_BYTES),
                base_offset: 0,
                end_offset: 0,
                reader_offset: 0,
                total_size: None,
                complete: false,
                error: None,
                requested_seek: None,
                resume_threshold: PLAYBACK_START_BUFFER_BYTES,
            }),
            changed: Condvar::new(),
            cancelled: AtomicBool::new(false),
            cancel_notify: Notify::new(),
        });

        let worker_shared = shared.clone();
        let url = url.to_owned();
        let worker = thread::Builder::new()
            .name("zmusic-http".to_owned())
            .spawn(move || download_worker(worker_shared, url))
            .map_err(|_| PlayerError::NetworkUnavailable)?;

        let stream = Self {
            shared,
            cursor: 0,
            worker: Some(worker),
        };
        stream.wait_initial_buffer()?;
        Ok(stream)
    }

    pub fn control(&self) -> HttpStreamControl {
        HttpStreamControl {
            shared: self.shared.clone(),
        }
    }

    pub fn cancel(&self) {
        cancel(&self.shared);
    }

    pub fn total_size(&self) -> Option<u64> {
        self.shared.state.lock().unwrap().total_size
    }

    pub fn error(&self) -> Option<PlayerError> {
        self.shared.state.lock().unwrap().error
    }

    fn wait_initial_buffer(&self) -> Result<(), PlayerError> {
        let deadline = Instant::now() + INITIAL_BUFFER_TIMEOUT;
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if let Some(error) = state.error {
                return Err(error);
            }
            if state.data.len() >= INITIAL_BUFFER_BYTES || state.complete {
                return if state.data.is_empty() {
                    Err(PlayerError::DecodeFailed)
                } else {
                    Ok(())
                };
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(PlayerError::StreamTimeout);
            }
            let timeout = deadline.saturating_duration_since(now);
            (state, _) = self.shared.changed.wait_timeout(state, timeout).unwrap();
        }
    }
}

impl HttpStreamControl {
    pub fn cancel(&self) {
        cancel(&self.shared);
    }

    pub fn error(&self) -> Option<PlayerError> {
        self.shared.state.lock().unwrap().error
    }
}

impl Read for HttpStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        read_from_buffer(&self.shared, &mut self.cursor, output)
    }
}

impl Seek for HttpStream {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        seek_in_buffer(&self.shared, &mut self.cursor, position)
    }
}

impl Drop for HttpStream {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn cancel(shared: &Shared) {
    shared.cancelled.store(true, Ordering::Release);
    shared.cancel_notify.notify_waiters();
    shared.changed.notify_all();
}

fn read_from_buffer(shared: &Shared, cursor: &mut u64, output: &mut [u8]) -> io::Result<usize> {
    if output.is_empty() {
        return Ok(0);
    }
    let mut state = shared.state.lock().unwrap();

    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(0);
        }
        if let Some(error) = state.error {
            return if error == PlayerError::Cancelled {
                Ok(0)
            } else {
                Err(io::Error::other(error))
            };
        }

        if *cursor >= state.base_offset && *cursor < state.end_offset {
            let start = (*cursor - state.base_offset) as usize;
            let available = (state.end_offset - *cursor) as usize;
            if state.resume_threshold == 0 || available >= state.resume_threshold || state.complete
            {
                state.resume_threshold = 0;
                let count = output.len().min(available);
                let data = state.data.make_contiguous();
                output[..count].copy_from_slice(&data[start..start + count]);
                *cursor += count as u64;
                state.reader_offset = *cursor;
                shared.changed.notify_all();
                return Ok(count);
            }
        }

        if *cursor == state.end_offset && state.complete {
            return Ok(0);
        }

        if *cursor < state.base_offset || *cursor > state.end_offset {
            state.requested_seek = Some(*cursor);
            state.complete = false;
            state.resume_threshold = REBUFFER_BYTES;
            shared.changed.notify_all();
        } else if *cursor == state.end_offset {
            state.resume_threshold = REBUFFER_BYTES;
        }

        (state, _) = shared
            .changed
            .wait_timeout(state, Duration::from_millis(100))
            .unwrap();
    }
}

fn seek_in_buffer(shared: &Shared, cursor: &mut u64, position: SeekFrom) -> io::Result<u64> {
    let mut state = shared.state.lock().unwrap();
    let target = match position {
        SeekFrom::Start(offset) => i128::from(offset),
        SeekFrom::Current(offset) => i128::from(*cursor) + i128::from(offset),
        SeekFrom::End(offset) => match state.total_size {
            Some(total_size) => i128::from(total_size) + i128::from(offset),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "流总长度未知，无法从末尾定位",
                ));
            }
        },
    };
    if target < 0 || target > i128::from(u64::MAX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "流定位超出有效范围",
        ));
    }
    let target = target as u64;
    if state
        .total_size
        .is_some_and(|total_size| target > total_size)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "流定位超过内容末尾",
        ));
    }

    *cursor = target;
    state.reader_offset = target;
    if target < state.base_offset || target > state.end_offset {
        state.requested_seek = Some(target);
        state.complete = false;
        state.resume_threshold = REBUFFER_BYTES;
    }
    shared.changed.notify_all();
    Ok(target)
}

fn download_worker(shared: Arc<Shared>, url: String) {
    let runtime = match Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(_) => {
            set_error(&shared, PlayerError::NetworkUnavailable);
            return;
        }
    };
    let worker_shared = shared.clone();
    if catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(download_loop(worker_shared, url));
    }))
    .is_err()
    {
        set_error(&shared, PlayerError::NetworkUnavailable);
    }
}

async fn download_loop(shared: Arc<Shared>, url: String) {
    if !ensure_tls_provider() {
        set_error(&shared, PlayerError::NetworkUnavailable);
        return;
    }
    let client = match Client::builder().connect_timeout(CONNECT_TIMEOUT).build() {
        Ok(client) => client,
        Err(_) => {
            set_error(&shared, PlayerError::NetworkUnavailable);
            return;
        }
    };

    // 解码器 seek 会替换缓冲；网络重连必须保留尚未消费的数据并接在缓冲末尾。
    let mut start_offset = 0_u64;
    let mut preserve_buffer = false;
    let mut reconnect_attempts = 0_usize;
    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            set_error(&shared, PlayerError::Cancelled);
            return;
        }

        let mut request = client.get(&url);
        if start_offset > 0 {
            request = request.header(RANGE, format!("bytes={start_offset}-"));
        }

        let send_result = tokio::select! {
            result = timeout(RESPONSE_HEADER_TIMEOUT, request.send()) => result,
            () = wait_cancelled(&shared) => {
                set_error(&shared, PlayerError::Cancelled);
                return;
            }
        };
        let mut response = match send_result {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                let error = if error.is_timeout() {
                    PlayerError::StreamTimeout
                } else {
                    PlayerError::HttpError
                };
                if preserve_buffer {
                    let Some((offset, keep_buffer)) = next_request_after_stream_error(
                        &shared,
                        error,
                        start_offset,
                        &mut reconnect_attempts,
                    ) else {
                        return;
                    };
                    start_offset = offset;
                    preserve_buffer = keep_buffer;
                    if keep_buffer && !wait_before_reconnect(&shared).await {
                        return;
                    }
                    continue;
                }
                set_error(&shared, error);
                return;
            }
            Err(_) => {
                if preserve_buffer {
                    let Some((offset, keep_buffer)) = next_request_after_stream_error(
                        &shared,
                        PlayerError::StreamTimeout,
                        start_offset,
                        &mut reconnect_attempts,
                    ) else {
                        return;
                    };
                    start_offset = offset;
                    preserve_buffer = keep_buffer;
                    if keep_buffer && !wait_before_reconnect(&shared).await {
                        return;
                    }
                    continue;
                }
                set_error(&shared, PlayerError::StreamTimeout);
                return;
            }
        };

        if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            if !range_not_satisfiable_is_eof(&shared, &response, start_offset) {
                set_error(&shared, PlayerError::HttpError);
                return;
            }
            mark_complete(&shared, start_offset);
            match wait_for_seek(&shared) {
                Some(offset) => {
                    start_offset = offset;
                    preserve_buffer = false;
                    continue;
                }
                None => return,
            }
        }
        if !response.status().is_success() {
            set_error(&shared, PlayerError::HttpError);
            return;
        }

        let partial = response.status() == StatusCode::PARTIAL_CONTENT;
        if partial && parse_content_range_start(&response) != Some(start_offset) {
            set_error(&shared, PlayerError::HttpError);
            return;
        }
        let response_start = if partial { start_offset } else { 0 };
        let total_size = parse_total_size(&response).or_else(|| {
            response
                .content_length()
                .map(|length| response_start.saturating_add(length))
        });
        if preserve_buffer {
            if !resume_download(&shared, start_offset, total_size) {
                return;
            }
        } else {
            reset_download(&shared, start_offset, total_size);
        }

        let mut wire_offset = response_start;
        let mut discard_until = if partial {
            response_start
        } else {
            start_offset
        };
        let mut restart = None;

        loop {
            if shared.cancelled.load(Ordering::Acquire) {
                set_error(&shared, PlayerError::Cancelled);
                return;
            }
            if let Some(offset) = take_requested_seek(&shared) {
                restart = Some((offset, false));
                break;
            }

            let chunk_result = tokio::select! {
                result = timeout(NETWORK_STALL_TIMEOUT, response.chunk()) => result,
                () = wait_cancelled(&shared) => {
                    set_error(&shared, PlayerError::Cancelled);
                    return;
                }
            };
            // 服务端忽略 Range 时，wire_offset 可能仍落后于已经组装好的缓冲末尾。
            let chunk = match chunk_result {
                Ok(Ok(None)) => {
                    mark_complete(&shared, wire_offset);
                    break;
                }
                Ok(Ok(Some(chunk))) => chunk,
                Err(_) => {
                    restart = next_request_after_stream_error(
                        &shared,
                        PlayerError::StreamTimeout,
                        wire_offset.max(discard_until),
                        &mut reconnect_attempts,
                    );
                    break;
                }
                Ok(Err(_)) => {
                    restart = next_request_after_stream_error(
                        &shared,
                        PlayerError::HttpError,
                        wire_offset.max(discard_until),
                        &mut reconnect_attempts,
                    );
                    break;
                }
            };

            let count = chunk.len();
            let chunk_start = wire_offset;
            wire_offset = wire_offset.saturating_add(count as u64);
            let mut slice = chunk.as_ref();
            if chunk_start < discard_until {
                let skipped = (discard_until - chunk_start).min(count as u64) as usize;
                slice = &slice[skipped..];
            }
            if !slice.is_empty() && !append_chunk(&shared, slice) {
                return;
            }
            discard_until = discard_until.max(wire_offset.min(discard_until));
        }

        if let Some((offset, keep_buffer)) = restart {
            start_offset = offset;
            preserve_buffer = keep_buffer;
            if keep_buffer && !wait_before_reconnect(&shared).await {
                return;
            }
            continue;
        }

        if shared.state.lock().unwrap().error.is_some() {
            return;
        }

        match wait_for_seek(&shared) {
            Some(offset) => {
                start_offset = offset;
                preserve_buffer = false;
            }
            None => return,
        }
    }
}

fn ensure_tls_provider() -> bool {
    *TLS_PROVIDER_READY.get_or_init(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        rustls::crypto::CryptoProvider::get_default().is_some()
    })
}

async fn wait_cancelled(shared: &Shared) {
    let notified = shared.cancel_notify.notified();
    if shared.cancelled.load(Ordering::Acquire) {
        return;
    }
    notified.await;
}

async fn wait_before_reconnect(shared: &Shared) -> bool {
    tokio::select! {
        () = tokio::time::sleep(RECONNECT_BACKOFF) => true,
        () = wait_cancelled(shared) => false,
    }
}

fn reset_download(shared: &Shared, start_offset: u64, total_size: Option<u64>) {
    let mut state = shared.state.lock().unwrap();
    state.data.clear();
    state.base_offset = start_offset;
    state.end_offset = start_offset;
    state.complete = false;
    state.error = None;
    if total_size.is_some() {
        state.total_size = total_size;
    }
    shared.changed.notify_all();
}

fn resume_download(shared: &Shared, start_offset: u64, total_size: Option<u64>) -> bool {
    let mut state = shared.state.lock().unwrap();
    if state.end_offset != start_offset
        || matches!((state.total_size, total_size), (Some(old), Some(new)) if old != new)
    {
        state.error = Some(PlayerError::HttpError);
        state.complete = true;
        shared.changed.notify_all();
        return false;
    }
    state.complete = false;
    state.error = None;
    if total_size.is_some() {
        state.total_size = total_size;
    }
    shared.changed.notify_all();
    true
}

fn append_chunk(shared: &Shared, mut chunk: &[u8]) -> bool {
    let mut state = shared.state.lock().unwrap();
    while !chunk.is_empty() {
        if shared.cancelled.load(Ordering::Acquire) {
            return false;
        }
        if state.requested_seek.is_some() {
            return true;
        }

        let free = MAX_BUFFER_BYTES - state.data.len();
        if free == 0 {
            let discard_limit = state.reader_offset.saturating_sub(REWIND_BYTES);
            let discardable = discard_limit.saturating_sub(state.base_offset) as usize;
            let discard = discardable.min(state.data.len());
            if discard > 0 {
                state.data.drain(..discard);
                state.base_offset += discard as u64;
                continue;
            }
            (state, _) = shared
                .changed
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap();
            continue;
        }

        let count = free.min(chunk.len());
        state.data.extend(&chunk[..count]);
        state.end_offset += count as u64;
        chunk = &chunk[count..];
        shared.changed.notify_all();
    }
    true
}

fn take_requested_seek(shared: &Shared) -> Option<u64> {
    shared.state.lock().unwrap().requested_seek.take()
}

fn next_request_after_stream_error(
    shared: &Shared,
    error: PlayerError,
    resume_offset: u64,
    reconnect_attempts: &mut usize,
) -> Option<(u64, bool)> {
    let mut state = shared.state.lock().unwrap();
    if let Some(offset) = state.requested_seek.take() {
        return Some((offset, false));
    }
    if *reconnect_attempts < MAX_RECONNECT_ATTEMPTS {
        *reconnect_attempts += 1;
        return Some((resume_offset, true));
    }
    state.error = Some(error);
    state.complete = true;
    shared.changed.notify_all();
    None
}

fn wait_for_seek(shared: &Shared) -> Option<u64> {
    let mut state = shared.state.lock().unwrap();
    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            return None;
        }
        if let Some(offset) = state.requested_seek.take() {
            return Some(offset);
        }
        state = shared.changed.wait(state).unwrap();
    }
}

fn mark_complete(shared: &Shared, end_offset: u64) {
    let mut state = shared.state.lock().unwrap();
    state.end_offset = state.end_offset.max(end_offset);
    state.complete = true;
    shared.changed.notify_all();
}

fn set_error(shared: &Shared, error: PlayerError) {
    let mut state = shared.state.lock().unwrap();
    state.error = Some(error);
    state.complete = true;
    shared.changed.notify_all();
}

fn parse_total_size(response: &reqwest::Response) -> Option<u64> {
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    value.rsplit_once('/')?.1.parse().ok()
}

fn parse_content_range_start(response: &reqwest::Response) -> Option<u64> {
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let (unit, range) = value.trim().split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    range.split_once('-')?.0.parse().ok()
}

fn range_not_satisfiable_is_eof(
    shared: &Shared,
    response: &reqwest::Response,
    start_offset: u64,
) -> bool {
    let reported_size = parse_total_size(response);
    let known_size = shared.state.lock().unwrap().total_size;
    if matches!((known_size, reported_size), (Some(old), Some(new)) if old != new) {
        return false;
    }
    reported_size
        .or(known_size)
        .is_some_and(|total_size| start_offset >= total_size)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    const EXPECTED_PLAYBACK_START_BUFFER_BYTES: usize = 256 * 1024;
    const EXPECTED_REBUFFER_BYTES: usize = 256 * 1024;

    fn shared_with_data(data_len: usize) -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(BufferState {
                data: VecDeque::from(vec![7_u8; data_len]),
                base_offset: 0,
                end_offset: data_len as u64,
                reader_offset: 0,
                total_size: None,
                complete: false,
                error: None,
                requested_seek: None,
                resume_threshold: PLAYBACK_START_BUFFER_BYTES,
            }),
            changed: Condvar::new(),
            cancelled: AtomicBool::new(false),
            cancel_notify: Notify::new(),
        })
    }

    fn append_test_data(shared: &Shared, count: usize) {
        let mut state = shared.state.lock().unwrap();
        state.data.extend(vec![9_u8; count]);
        state.end_offset += count as u64;
        shared.changed.notify_all();
    }

    fn read_request(socket: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1_024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
        }
        String::from_utf8(request).unwrap()
    }

    fn read_to_end(mut stream: HttpStream) -> io::Result<Vec<u8>> {
        let mut result = Vec::new();
        stream.read_to_end(&mut result)?;
        Ok(result)
    }

    fn parse_range_start(request: &str) -> Option<usize> {
        request.lines().find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("range: bytes=")
                .and_then(|value| value.strip_suffix('-'))
                .and_then(|value| value.parse().ok())
        })
    }

    #[test]
    fn invalid_url_is_rejected() {
        assert!(matches!(
            HttpStream::start("file.mp3"),
            Err(PlayerError::InvalidArgument)
        ));
    }

    #[test]
    fn first_read_waits_for_playback_start_buffer() {
        let shared = shared_with_data(INITIAL_BUFFER_BYTES);
        let reader_shared = shared.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut cursor = 0;
            let mut output = [0_u8; 1];
            started_tx.send(()).unwrap();
            result_tx
                .send(read_from_buffer(&reader_shared, &mut cursor, &mut output))
                .unwrap();
        });

        started_rx.recv().unwrap();
        let early_result = result_rx.recv_timeout(Duration::from_millis(100));
        append_test_data(
            &shared,
            EXPECTED_PLAYBACK_START_BUFFER_BYTES - INITIAL_BUFFER_BYTES,
        );
        let returned_early = early_result.is_ok();
        let result = early_result.unwrap_or_else(|_| {
            result_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("达到首缓冲水位后读取未恢复")
        });
        reader.join().unwrap();

        assert!(!returned_early, "首缓冲不足时不应开始解码");
        assert_eq!(result.unwrap(), 1);
    }

    #[test]
    fn underrun_waits_for_rebuffer_reserve() {
        let shared = shared_with_data(EXPECTED_PLAYBACK_START_BUFFER_BYTES);
        let reader_shared = shared.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut cursor = EXPECTED_PLAYBACK_START_BUFFER_BYTES as u64;
            let mut output = [0_u8; 1];
            started_tx.send(()).unwrap();
            result_tx
                .send(read_from_buffer(&reader_shared, &mut cursor, &mut output))
                .unwrap();
        });

        started_rx.recv().unwrap();
        assert!(result_rx.recv_timeout(Duration::from_millis(50)).is_err());
        append_test_data(&shared, 64 * 1024);
        let early_result = result_rx.recv_timeout(Duration::from_millis(100));
        append_test_data(&shared, EXPECTED_REBUFFER_BYTES - 64 * 1024);
        let returned_early = early_result.is_ok();
        let result = early_result.unwrap_or_else(|_| {
            result_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("达到重缓冲水位后读取未恢复")
        });
        reader.join().unwrap();

        assert!(!returned_early, "少量网络数据不应立即恢复解码");
        assert_eq!(result.unwrap(), 1);
    }

    #[test]
    fn response_without_content_length_streams_to_eof() {
        let payload: Vec<u8> = (0..192 * 1024).map(|index| (index % 251) as u8).collect();
        let expected = payload.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            read_request(&mut socket);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .unwrap();
            socket.write_all(&payload).unwrap();
        });

        let stream = HttpStream::start(&format!("http://{address}/audio.bin")).unwrap();
        assert_eq!(stream.total_size(), None);
        assert_eq!(read_to_end(stream).unwrap(), expected);
        server.join().unwrap();
    }

    #[test]
    fn chunked_response_streams_to_eof() {
        let payload: Vec<u8> = (0..192 * 1024).map(|index| (index % 239) as u8).collect();
        let expected = payload.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            read_request(&mut socket);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            for chunk in payload.chunks(24 * 1024) {
                write!(socket, "{:X}\r\n", chunk.len()).unwrap();
                socket.write_all(chunk).unwrap();
                socket.write_all(b"\r\n").unwrap();
            }
            socket.write_all(b"0\r\n\r\n").unwrap();
        });

        let stream = HttpStream::start(&format!("http://{address}/audio.bin")).unwrap();
        assert_eq!(stream.total_size(), None);
        assert_eq!(read_to_end(stream).unwrap(), expected);
        server.join().unwrap();
    }

    #[test]
    fn redirect_is_followed() {
        let payload = vec![42_u8; 192 * 1024];
        let expected = payload.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut redirect_socket, _) = listener.accept().unwrap();
            read_request(&mut redirect_socket);
            write!(
                redirect_socket,
                "HTTP/1.1 302 Found\r\nLocation: http://{address}/audio.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            drop(redirect_socket);

            let (mut audio_socket, _) = listener.accept().unwrap();
            read_request(&mut audio_socket);
            write!(
                audio_socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            audio_socket.write_all(&payload).unwrap();
        });

        let stream = HttpStream::start(&format!("http://{address}/redirect")).unwrap();
        assert_eq!(read_to_end(stream).unwrap(), expected);
        server.join().unwrap();
    }

    #[test]
    fn body_gap_longer_than_old_timeout_does_not_fail() {
        let payload = vec![7_u8; 256 * 1024];
        let expected = payload.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            read_request(&mut socket);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            socket.write_all(&payload[..160 * 1024]).unwrap();
            socket.flush().unwrap();
            thread::sleep(Duration::from_millis(750));
            socket.write_all(&payload[160 * 1024..]).unwrap();
        });

        let stream = HttpStream::start(&format!("http://{address}/audio.bin")).unwrap();
        assert_eq!(read_to_end(stream).unwrap(), expected);
        server.join().unwrap();
    }

    #[test]
    fn stalled_response_is_cancelled_within_one_second() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2_048];
            let _ = socket.read(&mut request);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n")
                .unwrap();
            socket.write_all(&vec![0_u8; 256 * 1024]).unwrap();
            socket.flush().unwrap();
            thread::sleep(Duration::from_secs(3));
        });

        let stream = HttpStream::start(&format!("http://{address}/audio.mp3")).unwrap();
        let started = Instant::now();
        drop(stream);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "停滞连接取消耗时 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn stalled_response_headers_time_out_within_three_seconds() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (_socket, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(3));
        });

        let started = Instant::now();
        assert!(matches!(
            HttpStream::start(&format!("http://{address}/audio.mp3")),
            Err(PlayerError::StreamTimeout)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "响应头超时耗时 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn seek_beyond_buffer_restarts_download_with_range() {
        let payload: Vec<u8> = (0..MAX_BUFFER_BYTES + 512 * 1024)
            .map(|index| (index % 251) as u8)
            .collect();
        let expected_payload = payload.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_requests = requests.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let request = read_request(&mut socket);
                let range_start = parse_range_start(&request);
                server_requests.lock().unwrap().push(request);

                let start = range_start.unwrap_or(0);
                let status = if range_start.is_some() {
                    "206 Partial Content"
                } else {
                    "200 OK"
                };
                let content_range = range_start.map_or_else(String::new, |offset| {
                    format!(
                        "Content-Range: bytes {offset}-{}/{}\r\n",
                        payload.len() - 1,
                        payload.len()
                    )
                });
                let headers = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{content_range}Connection: close\r\n\r\n",
                    payload.len() - start
                );
                if socket.write_all(headers.as_bytes()).is_ok() {
                    let _ = socket.write_all(&payload[start..]);
                }
            }
        });

        let mut stream = HttpStream::start(&format!("http://{address}/audio.mp3")).unwrap();
        let target = (MAX_BUFFER_BYTES + 128 * 1024) as u64;
        assert_eq!(stream.seek(SeekFrom::Start(target)).unwrap(), target);
        let mut output = [0_u8; 64];
        stream.read_exact(&mut output).unwrap();
        assert_eq!(
            output.as_slice(),
            &expected_payload[target as usize..target as usize + output.len()]
        );
        drop(stream);
        server.join().unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains(&format!("range: bytes={target}-"))
        );
    }

    #[test]
    fn seek_restarts_after_old_response_is_truncated() {
        let payload: Vec<u8> = (0..MAX_BUFFER_BYTES + 512 * 1024)
            .map(|index| (index % 251) as u8)
            .collect();
        let expected_payload = payload.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (truncate_tx, truncate_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut initial_socket, _) = listener.accept().unwrap();
            read_request(&mut initial_socket);
            write!(
                initial_socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            initial_socket
                .write_all(&payload[..INITIAL_BUFFER_BYTES])
                .unwrap();
            initial_socket.flush().unwrap();
            truncate_rx.recv().unwrap();
            drop(initial_socket);

            let (mut range_socket, _) = listener.accept().unwrap();
            let request = read_request(&mut range_socket);
            let range_start = parse_range_start(&request).unwrap();
            write!(
                range_socket,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {range_start}-{}/{}\r\nConnection: close\r\n\r\n",
                payload.len() - range_start,
                payload.len() - 1,
                payload.len()
            )
            .unwrap();
            range_socket.write_all(&payload[range_start..]).unwrap();
        });

        let mut stream = HttpStream::start(&format!("http://{address}/audio.mp3")).unwrap();
        thread::sleep(Duration::from_millis(20));
        let target = (MAX_BUFFER_BYTES + 128 * 1024) as u64;
        assert_eq!(stream.seek(SeekFrom::Start(target)).unwrap(), target);
        truncate_tx.send(()).unwrap();

        let mut output = [0_u8; 64];
        stream.read_exact(&mut output).unwrap();
        assert_eq!(
            output.as_slice(),
            &expected_payload[target as usize..target as usize + output.len()]
        );
        drop(stream);
        server.join().unwrap();
    }

    #[test]
    fn truncated_response_resumes_from_received_offset() {
        let split = 192 * 1024;
        let payload: Vec<u8> = (0..512 * 1024).map(|index| (index % 251) as u8).collect();
        let expected_payload = payload.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (body_sent_tx, body_sent_rx) = mpsc::channel();
        let (disconnect_tx, disconnect_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut initial_socket, _) = listener.accept().unwrap();
            read_request(&mut initial_socket);
            write!(
                initial_socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            initial_socket.write_all(&payload[..split]).unwrap();
            initial_socket.flush().unwrap();
            body_sent_tx.send(()).unwrap();
            disconnect_rx.recv().unwrap();
            drop(initial_socket);

            let (mut range_socket, _) = listener.accept().unwrap();
            let request = read_request(&mut range_socket);
            let range_start = parse_range_start(&request).unwrap();
            assert_eq!(range_start, split);
            write!(
                range_socket,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {range_start}-{}/{}\r\nConnection: close\r\n\r\n",
                payload.len() - range_start,
                payload.len() - 1,
                payload.len()
            )
            .unwrap();
            range_socket.write_all(&payload[range_start..]).unwrap();
        });

        let stream = HttpStream::start(&format!("http://{address}/audio.bin")).unwrap();
        body_sent_rx.recv().unwrap();
        disconnect_tx.send(()).unwrap();
        let result = read_to_end(stream);

        assert_eq!(result.unwrap(), expected_payload);
        server.join().unwrap();
    }

    #[test]
    fn truncated_response_resumes_when_server_ignores_range() {
        let split = 192 * 1024;
        let payload: Vec<u8> = (0..512 * 1024).map(|index| (index % 239) as u8).collect();
        let expected_payload = payload.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (body_sent_tx, body_sent_rx) = mpsc::channel();
        let (disconnect_tx, disconnect_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut initial_socket, _) = listener.accept().unwrap();
            read_request(&mut initial_socket);
            write!(
                initial_socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            initial_socket.write_all(&payload[..split]).unwrap();
            initial_socket.flush().unwrap();
            body_sent_tx.send(()).unwrap();
            disconnect_rx.recv().unwrap();
            drop(initial_socket);

            let (mut retry_socket, _) = listener.accept().unwrap();
            let request = read_request(&mut retry_socket);
            assert_eq!(parse_range_start(&request), Some(split));
            write!(
                retry_socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            retry_socket.write_all(&payload).unwrap();
        });

        let stream = HttpStream::start(&format!("http://{address}/audio.bin")).unwrap();
        body_sent_rx.recv().unwrap();
        disconnect_tx.send(()).unwrap();
        let result = read_to_end(stream);

        assert_eq!(result.unwrap(), expected_payload);
        server.join().unwrap();
    }

    #[test]
    fn repeated_disconnect_while_discarding_prefix_keeps_assembled_offset() {
        let split = 192 * 1024;
        let payload: Vec<u8> = (0..512 * 1024).map(|index| (index % 233) as u8).collect();
        let expected_payload = payload.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (body_sent_tx, body_sent_rx) = mpsc::channel();
        let (disconnect_tx, disconnect_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut initial_socket, _) = listener.accept().unwrap();
            read_request(&mut initial_socket);
            write!(
                initial_socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            initial_socket.write_all(&payload[..split]).unwrap();
            initial_socket.flush().unwrap();
            body_sent_tx.send(()).unwrap();
            disconnect_rx.recv().unwrap();
            drop(initial_socket);

            let (mut full_socket, _) = listener.accept().unwrap();
            let request = read_request(&mut full_socket);
            assert_eq!(parse_range_start(&request), Some(split));
            write!(
                full_socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            full_socket.write_all(&payload[..split / 2]).unwrap();
            drop(full_socket);

            let (mut range_socket, _) = listener.accept().unwrap();
            let request = read_request(&mut range_socket);
            let range_start = parse_range_start(&request).unwrap();
            assert_eq!(range_start, split);
            write!(
                range_socket,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {range_start}-{}/{}\r\nConnection: close\r\n\r\n",
                payload.len() - range_start,
                payload.len() - 1,
                payload.len()
            )
            .unwrap();
            range_socket.write_all(&payload[range_start..]).unwrap();
        });

        let stream = HttpStream::start(&format!("http://{address}/audio.bin")).unwrap();
        body_sent_rx.recv().unwrap();
        disconnect_tx.send(()).unwrap();
        let result = read_to_end(stream);

        assert_eq!(result.unwrap(), expected_payload);
        server.join().unwrap();
    }

    #[test]
    fn reconnect_attempts_are_bounded() {
        let split = 192 * 1024;
        let total_size = 512 * 1024;
        let payload = vec![7_u8; split];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (body_sent_tx, body_sent_rx) = mpsc::channel();
        let (disconnect_tx, disconnect_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut initial_socket, _) = listener.accept().unwrap();
            read_request(&mut initial_socket);
            write!(
                initial_socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {total_size}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            initial_socket.write_all(&payload).unwrap();
            initial_socket.flush().unwrap();
            body_sent_tx.send(()).unwrap();
            disconnect_rx.recv().unwrap();
            drop(initial_socket);

            for _ in 0..MAX_RECONNECT_ATTEMPTS {
                let (mut retry_socket, _) = listener.accept().unwrap();
                let request = read_request(&mut retry_socket);
                let range_start = parse_range_start(&request).unwrap();
                assert_eq!(range_start, split);
                write!(
                    retry_socket,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {range_start}-{}/{}\r\nConnection: close\r\n\r\n",
                    total_size - range_start,
                    total_size - 1,
                    total_size
                )
                .unwrap();
            }
        });

        let mut stream = HttpStream::start(&format!("http://{address}/audio.bin")).unwrap();
        let control = stream.control();
        body_sent_rx.recv().unwrap();
        disconnect_tx.send(()).unwrap();
        let mut output = Vec::new();
        let result = stream.read_to_end(&mut output);

        assert!(result.is_err());
        assert_eq!(control.error(), Some(PlayerError::HttpError));
        server.join().unwrap();
    }

    #[test]
    fn mismatched_content_range_is_rejected() {
        let split = 192 * 1024;
        let payload = vec![11_u8; 512 * 1024];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (body_sent_tx, body_sent_rx) = mpsc::channel();
        let (disconnect_tx, disconnect_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut initial_socket, _) = listener.accept().unwrap();
            read_request(&mut initial_socket);
            write!(
                initial_socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            initial_socket.write_all(&payload[..split]).unwrap();
            initial_socket.flush().unwrap();
            body_sent_tx.send(()).unwrap();
            disconnect_rx.recv().unwrap();
            drop(initial_socket);

            let (mut retry_socket, _) = listener.accept().unwrap();
            let request = read_request(&mut retry_socket);
            assert_eq!(parse_range_start(&request), Some(split));
            write!(
                retry_socket,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 0-{}/{}\r\nConnection: close\r\n\r\n",
                payload.len(),
                payload.len() - 1,
                payload.len()
            )
            .unwrap();
        });

        let mut stream = HttpStream::start(&format!("http://{address}/audio.bin")).unwrap();
        let control = stream.control();
        body_sent_rx.recv().unwrap();
        disconnect_tx.send(()).unwrap();
        let mut output = Vec::new();
        let result = stream.read_to_end(&mut output);

        assert!(result.is_err());
        assert_eq!(control.error(), Some(PlayerError::HttpError));
        server.join().unwrap();
    }

    #[test]
    fn premature_range_not_satisfiable_is_rejected() {
        let split = 192 * 1024;
        let total_size = 512 * 1024;
        let payload = vec![13_u8; split];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (body_sent_tx, body_sent_rx) = mpsc::channel();
        let (disconnect_tx, disconnect_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut initial_socket, _) = listener.accept().unwrap();
            read_request(&mut initial_socket);
            write!(
                initial_socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {total_size}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            initial_socket.write_all(&payload).unwrap();
            initial_socket.flush().unwrap();
            body_sent_tx.send(()).unwrap();
            disconnect_rx.recv().unwrap();
            drop(initial_socket);

            let (mut retry_socket, _) = listener.accept().unwrap();
            let request = read_request(&mut retry_socket);
            assert_eq!(parse_range_start(&request), Some(split));
            write!(
                retry_socket,
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total_size}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });

        let mut stream = HttpStream::start(&format!("http://{address}/audio.bin")).unwrap();
        let control = stream.control();
        body_sent_rx.recv().unwrap();
        disconnect_tx.send(()).unwrap();
        let mut output = Vec::new();
        let result = stream.read_to_end(&mut output);

        assert!(result.is_err());
        assert_eq!(control.error(), Some(PlayerError::HttpError));
        server.join().unwrap();
    }
}
