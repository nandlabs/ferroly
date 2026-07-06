//! WebSocket client: in-house upgrade handshake and framing over `ferroly::http`.

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use ferroly::http::transport::{connect, tls_config};
use ferroly::http::Uri;

use super::conn::{read_loop, write_loop, Outgoing, Reader};
use super::{handshake, rand, Message, WsError, WsOptions};

/// A connected WebSocket client.
///
/// Reads and writes are decoupled by background tasks and channels, so
/// [`send`](WsClient::send) may be called concurrently while [`recv`](WsClient::recv)
/// drains incoming messages.
pub struct WsClient {
    out: mpsc::UnboundedSender<Outgoing>,
    incoming: mpsc::UnboundedReceiver<Message>,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
}

impl WsClient {
    /// Connects to `url` (`ws://…` or `wss://…`), performing the RFC 6455
    /// opening handshake.
    pub async fn dial(url: &str, opts: WsOptions) -> Result<WsClient, WsError> {
        let uri = Uri::parse(url).map_err(|e| WsError::Connect(e.to_string()))?;
        let conn = connect(&uri, &tls_config())
            .await
            .map_err(|e| WsError::Connect(e.to_string()))?;

        let mut key_bytes = [0u8; 16];
        rand::fill(&mut key_bytes);
        let key = handshake::base64_encode(&key_bytes);

        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            uri.request_target(),
            uri.authority(),
            key,
        );

        let mut reader = BufReader::new(conn);
        reader
            .write_all(request.as_bytes())
            .await
            .map_err(|e| WsError::Connect(e.to_string()))?;
        reader
            .flush()
            .await
            .map_err(|e| WsError::Connect(e.to_string()))?;

        let (status, headers) = ferroly::http::io::read_response_head(&mut reader)
            .await
            .map_err(|e| WsError::Connect(e.to_string()))?;
        if status.as_u16() != 101 {
            return Err(WsError::Connect(format!(
                "expected 101, got {}",
                status.as_u16()
            )));
        }
        let expected = handshake::accept_key(&key);
        if headers.get("sec-websocket-accept") != Some(expected.as_str()) {
            return Err(WsError::Connect("invalid Sec-WebSocket-Accept".into()));
        }

        // Any bytes buffered past the handshake begin the frame stream.
        let leftover = reader.buffer().to_vec();
        let conn = reader.into_inner();
        let (read_half, write_half) = tokio::io::split(conn);

        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let reader = Reader::new(read_half, leftover, opts.max_frame_size);

        let reader_task = tokio::spawn(read_loop(
            reader,
            in_tx,
            out_tx.clone(),
            opts.max_message_size,
        ));
        let writer_task = tokio::spawn(write_loop(write_half, out_rx, true));

        Ok(WsClient {
            out: out_tx,
            incoming: in_rx,
            reader_task,
            writer_task,
        })
    }

    /// Sends a message.
    pub fn send(&self, msg: Message) -> Result<(), WsError> {
        self.out
            .send(Outgoing::Msg(msg))
            .map_err(|e| WsError::Send(e.to_string()))
    }

    /// Receives the next message, or `None` once the connection closes.
    pub async fn recv(&mut self) -> Option<Message> {
        self.incoming.recv().await
    }

    /// Sends a close frame and stops the background tasks.
    pub async fn close(mut self) -> Result<(), WsError> {
        let _ = self.out.send(Outgoing::Close);
        // Let the writer flush the close frame. Borrow rather than move the
        // handle so `Drop` (which aborts both tasks) can still run cleanly.
        let _ = (&mut self.writer_task).await;
        Ok(())
    }
}

impl Drop for WsClient {
    /// Aborts the background reader/writer tasks if the client is dropped without
    /// an explicit [`close`](WsClient::close), so a socket + parked read task
    /// cannot leak when the peer never sends another frame.
    fn drop(&mut self) {
        self.reader_task.abort();
        self.writer_task.abort();
    }
}
