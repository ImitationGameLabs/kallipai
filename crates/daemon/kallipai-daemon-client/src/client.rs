//! UDS client for the kallipai local daemon.
//!
//! One short-lived connection per call: connect → write the request line →
//! read one response line → drop. The socket's 0600 mode is the entire auth
//! story; there is no handshake.

use std::path::{Path, PathBuf};

use kallipai_daemon_common::wire::{
    Request, RequestBody, Response, decode_response, encode_request,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Everything that can go wrong on the client side of one exchange.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("connecting to {path}: {source}")]
    Connect {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing the request: {0}")]
    Write(#[from] std::io::Error),
    #[error("daemon closed the connection before answering")]
    Closed,
    #[error(
        "response exceeded {} bytes",
        kallipai_daemon_common::wire::MAX_LINE_BYTES
    )]
    TooLarge,
    #[error("malformed daemon response: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// A connected-on-call client. Cheap to construct; holds no state between
/// calls (each call is a fresh connection, matching the server's
/// one-exchange-per-connection model).
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket: PathBuf,
}

impl DaemonClient {
    /// Client for the daemon listening on `socket`.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Perform one request/response exchange.
    pub async fn call(&self, body: RequestBody) -> Result<Response, ClientError> {
        self.raw(&kallipai_daemon_common::wire::request(body)).await
    }

    /// Send an already-built envelope (useful for version-mismatch tests).
    pub async fn raw(&self, request: &Request) -> Result<Response, ClientError> {
        let line = encode_request(request)?;
        self.raw_line(&line).await
    }

    /// Send one literal request line (no envelope construction); protocol
    /// tests use this to feed the server deliberately broken input.
    pub async fn raw_line(&self, line: &str) -> Result<Response, ClientError> {
        let stream =
            UnixStream::connect(&self.socket)
                .await
                .map_err(|source| ClientError::Connect {
                    path: self.socket.clone(),
                    source,
                })?;
        let (reader, mut writer) = stream.into_split();

        let mut payload = line.as_bytes().to_vec();
        payload.push(b'\n');
        writer.write_all(&payload).await?;
        writer.flush().await?;

        let mut reader = BufReader::new(reader);
        let mut response_line = String::new();
        let read = reader
            .read_line(&mut response_line)
            .await
            .map_err(ClientError::Write)?;
        if read == 0 {
            return Err(ClientError::Closed);
        }
        if response_line.len() > kallipai_daemon_common::wire::MAX_LINE_BYTES {
            return Err(ClientError::TooLarge);
        }
        decode_response(response_line.trim_end()).map_err(ClientError::Malformed)
    }
}
