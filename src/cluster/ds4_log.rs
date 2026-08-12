use std::{io, sync::Arc};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader},
    sync::mpsc,
    task::JoinHandle,
};
use url::Url;

pub const MAX_CHILD_LOG_LINE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildLogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ds4LogEvent {
    HttpListening {
        url: Url,
    },
    DsparkActivated,
    WorkerRegistered {
        detail: String,
    },
    CompleteRouteReady {
        detail: String,
    },
    WorkerRemoved {
        detail: String,
    },
    RouteIncomplete {
        detail: String,
    },
    PrefillProgress {
        current: u64,
        total: u64,
        percent: f64,
        cached: u64,
    },
    KvCacheHit {
        tokens: u64,
        load_ms: f64,
    },
    GenerationProgress {
        completion: u64,
        chunk_tps: f64,
        avg_tps: f64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChildLogRecord {
    pub profile_id: Arc<str>,
    pub generation: u64,
    pub pid: u32,
    pub stream: ChildLogStream,
    pub line: String,
    pub truncated: bool,
    pub event: Option<Ds4LogEvent>,
}

#[derive(Debug)]
pub struct ChildLogForwarders {
    tasks: Vec<JoinHandle<io::Result<()>>>,
}

impl ChildLogForwarders {
    pub fn abort(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Drop for ChildLogForwarders {
    fn drop(&mut self) {
        self.abort();
    }
}

pub fn spawn_child_log_forwarders<Out, Err>(
    stdout: Out,
    stderr: Err,
    profile_id: Arc<str>,
    generation: u64,
    pid: u32,
    capacity: usize,
) -> (mpsc::Receiver<ChildLogRecord>, ChildLogForwarders)
where
    Out: AsyncRead + Unpin + Send + 'static,
    Err: AsyncRead + Unpin + Send + 'static,
{
    let (receiver, _, forwarders) = spawn_child_log_forwarders_inner(
        stdout, stderr, profile_id, generation, pid, capacity, false,
    );
    (receiver, forwarders)
}

pub fn spawn_child_log_forwarders_with_events<Out, Err>(
    stdout: Out,
    stderr: Err,
    profile_id: Arc<str>,
    generation: u64,
    pid: u32,
    capacity: usize,
) -> (
    mpsc::Receiver<ChildLogRecord>,
    mpsc::UnboundedReceiver<Ds4LogEvent>,
    ChildLogForwarders,
)
where
    Out: AsyncRead + Unpin + Send + 'static,
    Err: AsyncRead + Unpin + Send + 'static,
{
    let (logs, events, forwarders) = spawn_child_log_forwarders_inner(
        stdout, stderr, profile_id, generation, pid, capacity, true,
    );
    (logs, events.expect("event receiver requested"), forwarders)
}

fn spawn_child_log_forwarders_inner<Out, Err>(
    stdout: Out,
    stderr: Err,
    profile_id: Arc<str>,
    generation: u64,
    pid: u32,
    capacity: usize,
    capture_events: bool,
) -> (
    mpsc::Receiver<ChildLogRecord>,
    Option<mpsc::UnboundedReceiver<Ds4LogEvent>>,
    ChildLogForwarders,
)
where
    Out: AsyncRead + Unpin + Send + 'static,
    Err: AsyncRead + Unpin + Send + 'static,
{
    assert!(capacity > 0, "child log channel capacity must be positive");
    let (sender, receiver) = mpsc::channel(capacity);
    let (event_sender, event_receiver) = if capture_events {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Some(sender), Some(receiver))
    } else {
        (None, None)
    };
    let tasks = vec![
        tokio::spawn(forward_stream(
            stdout,
            sender.clone(),
            event_sender.clone(),
            profile_id.clone(),
            generation,
            pid,
            ChildLogStream::Stdout,
        )),
        tokio::spawn(forward_stream(
            stderr,
            sender,
            event_sender,
            profile_id,
            generation,
            pid,
            ChildLogStream::Stderr,
        )),
    ];
    (receiver, event_receiver, ChildLogForwarders { tasks })
}

pub fn parse_ds4_log_event(line: &str) -> Option<Ds4LogEvent> {
    let line = strip_timestamp(line);
    const LISTENING: &str = "ds4-server: listening on ";
    const REGISTERED: &str = "ds4: distributed coordinator: registered worker ";
    const COMPLETE: &str = "ds4: distributed coordinator: complete route ready: ";
    const REMOVED: &str = "ds4: distributed coordinator: removed worker ";
    const INCOMPLETE: &str = "ds4: distributed coordinator: route incomplete; ";
    const DSPARK_ACTIVATED: &str = "ds4: DSpark target-hidden capture enabled: layers=";

    if let Some(event) = parse_prefill_progress(line) {
        return Some(event);
    }
    if let Some(event) = parse_kv_cache_hit(line) {
        return Some(event);
    }
    if let Some(event) = parse_generation_progress(line) {
        return Some(event);
    }
    if let Some(value) = line.strip_prefix(LISTENING) {
        let url = Url::parse(value.trim()).ok()?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return None;
        }
        return Some(Ds4LogEvent::HttpListening { url });
    }
    if line
        .strip_prefix(DSPARK_ACTIVATED)
        .is_some_and(|layers| !layers.trim().is_empty())
    {
        return Some(Ds4LogEvent::DsparkActivated);
    }
    let parsed = [
        (REGISTERED, 0_u8),
        (COMPLETE, 1),
        (REMOVED, 2),
        (INCOMPLETE, 3),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| {
        let detail = line.strip_prefix(prefix)?.trim();
        (!detail.is_empty()).then(|| (kind, detail.to_owned()))
    })?;
    Some(match parsed {
        (0, detail) => Ds4LogEvent::WorkerRegistered { detail },
        (1, detail) => Ds4LogEvent::CompleteRouteReady { detail },
        (2, detail) => Ds4LogEvent::WorkerRemoved { detail },
        (_, detail) => Ds4LogEvent::RouteIncomplete { detail },
    })
}

/// Remove a `MMDD HH:MM:SS ` log prefix emitted by `server_log`.
fn strip_timestamp(line: &str) -> &str {
    let bytes = line.as_bytes();
    if bytes.len() >= 15
        && bytes[4] == b' '
        && bytes[7] == b':'
        && bytes[10] == b':'
        && bytes[13] == b' '
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[8..10].iter().all(|b| b.is_ascii_digit())
        && bytes[11..13].iter().all(|b| b.is_ascii_digit())
    {
        &line[14..]
    } else {
        line
    }
}

/// Parse `ds4-server: chat ctx=0..9005:0 prefill chunk 4096/9005 (45.5%) ...`.
fn parse_prefill_progress(line: &str) -> Option<Ds4LogEvent> {
    let marker = " prefill chunk ";
    let idx = line.find(marker)?;
    let before = &line[..idx];
    let after = &line[idx + marker.len()..];
    let (current_str, rest) = after.split_once('/')?;
    let current = current_str.trim().parse().ok()?;
    let (total_str, rest) = rest.split_once(' ')?;
    let total = total_str.trim().parse().ok()?;
    let percent_str = rest.trim_start().trim_start_matches('(').split_once('%')?.0;
    let percent = percent_str.trim().parse().ok()?;
    let cached = before
        .rsplit("ctx=")
        .next()?
        .split("..")
        .next()?
        .trim()
        .parse()
        .unwrap_or(0);
    Some(Ds4LogEvent::PrefillProgress {
        current,
        total,
        percent,
        cached,
    })
}

/// Parse `ds4: kv cache hit text tokens=9005 ... load=12.3 ms file=...`.
fn parse_kv_cache_hit(line: &str) -> Option<Ds4LogEvent> {
    let rest = line.strip_prefix("ds4: kv cache hit text ")?;
    let tokens = rest
        .strip_prefix("tokens=")?
        .split(' ')
        .next()?
        .parse()
        .ok()?;
    let load_ms = rest
        .split(" load=")
        .nth(1)?
        .split(' ')
        .next()?
        .parse()
        .ok()?;
    Some(Ds4LogEvent::KvCacheHit { tokens, load_ms })
}

/// Parse `ds4-server: chat ctx=... gen=42 ... decoding chunk=32.1 t/s avg=28.5 t/s ...`.
fn parse_generation_progress(line: &str) -> Option<Ds4LogEvent> {
    let rest = line.strip_prefix("ds4-server: ")?;
    let completion = rest
        .split(" gen=")
        .nth(1)?
        .split(' ')
        .next()?
        .parse()
        .ok()?;
    let chunk_tps = rest
        .split(" decoding chunk=")
        .nth(1)?
        .split(' ')
        .next()?
        .parse()
        .ok()?;
    let avg_tps = rest
        .split(" avg=")
        .nth(1)?
        .split(' ')
        .next()?
        .parse()
        .ok()?;
    Some(Ds4LogEvent::GenerationProgress {
        completion,
        chunk_tps,
        avg_tps,
    })
}

async fn forward_stream<R>(
    stream: R,
    sender: mpsc::Sender<ChildLogRecord>,
    event_sender: Option<mpsc::UnboundedSender<Ds4LogEvent>>,
    profile_id: Arc<str>,
    generation: u64,
    pid: u32,
    log_stream: ChildLogStream,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    while let Some((bytes, truncated)) = read_capped_line(&mut reader).await? {
        let line = String::from_utf8_lossy(&bytes).into_owned();
        let record = ChildLogRecord {
            profile_id: profile_id.clone(),
            generation,
            pid,
            stream: log_stream,
            event: (!truncated).then(|| parse_ds4_log_event(&line)).flatten(),
            line,
            truncated,
        };
        if let (Some(sender), Some(event)) = (&event_sender, record.event.clone()) {
            let _ = sender.send(event);
        }
        match sender.try_send(record) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
        }
    }
    Ok(())
}

async fn read_capped_line<R>(reader: &mut R) -> io::Result<Option<(Vec<u8>, bool)>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    let mut truncated = false;
    let mut saw_bytes = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if saw_bytes {
                Ok(Some((line, truncated)))
            } else {
                Ok(None)
            };
        }
        saw_bytes = true;
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        let chunk = &available[..end];
        let ends_with_newline = chunk.ends_with(b"\n");
        let remaining = MAX_CHILD_LOG_LINE_BYTES.saturating_sub(line.len());
        line.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        truncated |= chunk.len() > remaining;
        reader.consume(end);
        if ends_with_newline {
            if line.ends_with(b"\n") {
                line.pop();
                if line.ends_with(b"\r") {
                    line.pop();
                }
            }
            return Ok(Some((line, truncated)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_valid_recognized_events() {
        assert!(matches!(
            parse_ds4_log_event("ds4-server: listening on http://127.0.0.1:8080"),
            Some(Ds4LogEvent::HttpListening { .. })
        ));
        assert_eq!(
            parse_ds4_log_event("ds4: DSpark target-hidden capture enabled: layers=3,7,11"),
            Some(Ds4LogEvent::DsparkActivated)
        );
        assert_eq!(
            parse_ds4_log_event("ds4: distributed coordinator: registered worker node-b"),
            Some(Ds4LogEvent::WorkerRegistered {
                detail: "node-b".into()
            })
        );
        assert!(matches!(
            parse_ds4_log_event("ds4: distributed coordinator: complete route ready: deployment-7"),
            Some(Ds4LogEvent::CompleteRouteReady { .. })
        ));
        assert!(matches!(
            parse_ds4_log_event("ds4: distributed coordinator: removed worker node-b"),
            Some(Ds4LogEvent::WorkerRemoved { .. })
        ));
        assert!(matches!(
            parse_ds4_log_event("ds4: distributed coordinator: route incomplete; missing layer 20"),
            Some(Ds4LogEvent::RouteIncomplete { .. })
        ));
    }

    #[test]
    fn unknown_or_invalid_log_never_becomes_an_event() {
        assert_eq!(parse_ds4_log_event("model load complete"), None);
        assert_eq!(parse_ds4_log_event("ds4-server: listening on ..."), None);
        assert_eq!(
            parse_ds4_log_event("ds4: distributed coordinator: registered worker "),
            None
        );
    }

    #[test]
    fn parses_prefill_progress_with_and_without_timestamp() {
        assert_eq!(
            parse_ds4_log_event(
                "0812 14:30:45 ds4-server: chat ctx=0..9005:0 prefill chunk 4096/9005 (45.5%) chunk=123.4 t/s avg=100.0 t/s 10.000s",
            ),
            Some(Ds4LogEvent::PrefillProgress {
                current: 4096,
                total: 9005,
                percent: 45.5,
                cached: 0,
            })
        );
        assert_eq!(
            parse_ds4_log_event(
                "ds4-server: completion ctx=2048..9005:6957 prefill chunk 8192/9005 (91.0%) chunk=200.0 t/s avg=180.0 t/s 20.000s",
            ),
            Some(Ds4LogEvent::PrefillProgress {
                current: 8192,
                total: 9005,
                percent: 91.0,
                cached: 2048,
            })
        );
    }

    #[test]
    fn parses_kv_cache_hit() {
        assert_eq!(
            parse_ds4_log_event(
                "ds4: kv cache hit text tokens=9005 text=hello quant=8 key=prefix load=12.3 ms file=/tmp/cache.bin",
            ),
            Some(Ds4LogEvent::KvCacheHit {
                tokens: 9005,
                load_ms: 12.3,
            })
        );
    }

    #[test]
    fn parses_generation_progress() {
        assert_eq!(
            parse_ds4_log_event(
                "0812 14:31:00 ds4-server: chat ctx=0..9005:0 gen=42 decoding chunk=32.1 t/s avg=28.5 t/s 1.500s",
            ),
            Some(Ds4LogEvent::GenerationProgress {
                completion: 42,
                chunk_tps: 32.1,
                avg_tps: 28.5,
            })
        );
    }

    #[test]
    fn timestamped_listening_line_is_recognized() {
        assert!(matches!(
            parse_ds4_log_event("0812 14:29:00 ds4-server: listening on http://127.0.0.1:8000"),
            Some(Ds4LogEvent::HttpListening { .. })
        ));
    }

    #[tokio::test]
    async fn forwards_metadata_and_truncates_without_recognizing_partial_event() {
        let long = format!(
            "ds4: distributed coordinator: complete route ready: {}\nsecond\n",
            "x".repeat(MAX_CHILD_LOG_LINE_BYTES)
        );
        let (mut receiver, _tasks) = spawn_child_log_forwarders(
            std::io::Cursor::new(long),
            std::io::Cursor::new("stderr line\n"),
            Arc::from("mxfp4-coordinator"),
            8,
            42,
            8,
        );
        let mut records = Vec::new();
        while let Some(record) = receiver.recv().await {
            records.push(record);
            if records.len() == 3 {
                break;
            }
        }
        let truncated = records.iter().find(|record| record.truncated).unwrap();
        assert_eq!(truncated.line.len(), MAX_CHILD_LOG_LINE_BYTES);
        assert_eq!(truncated.event, None);
        assert_eq!(truncated.profile_id.as_ref(), "mxfp4-coordinator");
        assert_eq!(truncated.generation, 8);
        assert_eq!(truncated.pid, 42);
        assert!(records.iter().any(|record| {
            record.stream == ChildLogStream::Stderr && record.line == "stderr line"
        }));
    }

    #[tokio::test]
    async fn recognized_route_event_survives_a_full_lossy_log_queue() {
        let output = format!(
            "noise-1\nnoise-2\n{}\n",
            "ds4: distributed coordinator: complete route ready: deployment-9"
        );
        let (_logs, mut events, _tasks) = spawn_child_log_forwarders_with_events(
            std::io::Cursor::new(output),
            std::io::Cursor::new(Vec::<u8>::new()),
            Arc::from("mxfp4-coordinator"),
            9,
            43,
            1,
        );
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                .await
                .unwrap(),
            Some(Ds4LogEvent::CompleteRouteReady { detail }) if detail == "deployment-9"
        ));
    }
}
