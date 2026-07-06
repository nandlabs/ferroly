//! Line buffering for SSE / NDJSON provider streams, decoupled from any stream
//! library — fed by the in-house `ferroly::http::Response::chunk()` in provider tasks.

/// Upper bound on a single unterminated line before it is discarded — guards
/// against a malicious/broken SSE server that never emits a newline.
const MAX_BUFFERED_BYTES: usize = 1 << 20; // 1 MiB

/// Accumulates response bytes and yields complete lines across chunk boundaries.
pub(crate) struct LineBuffer {
    buf: String,
}

impl LineBuffer {
    pub(crate) fn new() -> Self {
        Self { buf: String::new() }
    }

    /// Appends a network chunk.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        // Drop an absurdly long unterminated line rather than buffering without
        // bound (a stream with no newline would otherwise grow memory forever).
        if self.buf.len() > MAX_BUFFERED_BYTES && !self.buf.contains('\n') {
            self.buf.clear();
        }
    }

    /// Removes and returns all complete (newline-terminated) lines, keeping any
    /// partial trailing line buffered.
    pub(crate) fn drain_lines(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=pos).collect();
            lines.push(line.trim_end().to_string());
        }
        lines
    }

    /// Consumes the buffer, returning any unterminated trailing line.
    pub(crate) fn take_remainder(self) -> String {
        self.buf.trim_end().to_string()
    }
}

/// Extracts the payload of an SSE `data:` line, or `None` for other lines.
pub(crate) fn sse_data(line: &str) -> Option<&str> {
    line.strip_prefix("data:").map(str::trim)
}

/// Reads a streaming HTTP response body chunk-by-chunk, parses each complete
/// line with `parse`, and forwards resulting chunks over `tx`. Runs to
/// completion inside a spawned task; returns early if the receiver is dropped.
pub(crate) async fn pump<F>(
    mut resp: ferroly::http::Response,
    tx: tokio::sync::mpsc::Sender<
        Result<ferroly::genai::CompletionChunk, ferroly::genai::GenAiError>,
    >,
    parse: F,
) where
    F: Fn(&str) -> Option<Result<ferroly::genai::CompletionChunk, ferroly::genai::GenAiError>>,
{
    let mut lb = LineBuffer::new();
    loop {
        match resp.chunk().await {
            Ok(Some(bytes)) => {
                lb.push(&bytes);
                for line in lb.drain_lines() {
                    if let Some(chunk) = parse(&line) {
                        if tx.send(chunk).await.is_err() {
                            return;
                        }
                    }
                }
            }
            Ok(None) => {
                let rem = lb.take_remainder();
                if !rem.is_empty() {
                    if let Some(chunk) = parse(&rem) {
                        let _ = tx.send(chunk).await;
                    }
                }
                return;
            }
            Err(e) => {
                let _ = tx
                    .send(Err(ferroly::genai::GenAiError::Transport(e.to_string())))
                    .await;
                return;
            }
        }
    }
}
