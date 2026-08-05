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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ds4LogEvent {
    HttpListening { url: Url },
    WorkerRegistered { detail: String },
    CompleteRouteReady { detail: String },
    WorkerRemoved { detail: String },
    RouteIncomplete { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    assert!(capacity > 0, "child log channel capacity must be positive");
    let (sender, receiver) = mpsc::channel(capacity);
    let tasks = vec![
        tokio::spawn(forward_stream(
            stdout,
            sender.clone(),
            profile_id.clone(),
            generation,
            pid,
            ChildLogStream::Stdout,
        )),
        tokio::spawn(forward_stream(
            stderr,
            sender,
            profile_id,
            generation,
            pid,
            ChildLogStream::Stderr,
        )),
    ];
    (receiver, ChildLogForwarders { tasks })
}

pub fn parse_ds4_log_event(line: &str) -> Option<Ds4LogEvent> {
    const LISTENING: &str = "ds4-server: listening on ";
    const REGISTERED: &str = "ds4: distributed coordinator: registered worker ";
    const COMPLETE: &str = "ds4: distributed coordinator: complete route ready: ";
    const REMOVED: &str = "ds4: distributed coordinator: removed worker ";
    const INCOMPLETE: &str = "ds4: distributed coordinator: route incomplete; ";

    if let Some(value) = line.strip_prefix(LISTENING) {
        let url = Url::parse(value.trim()).ok()?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return None;
        }
        return Some(Ds4LogEvent::HttpListening { url });
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

async fn forward_stream<R>(
    stream: R,
    sender: mpsc::Sender<ChildLogRecord>,
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
}
