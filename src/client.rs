//! Talking to a running daemon.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::config;
use serde::de::DeserializeOwned;

use crate::protocol::{Activity, BuildRequest, Failures, Message, Request, Status};

/// Connects to the daemon of `home`.
///
/// # Errors
/// `NotFound` or `ConnectionRefused` when no daemon runs.
pub fn connect(home: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(config::socket(home))
}

/// Asks for `request` and passes each message to `on_message`. Returns the
/// last one, [`Message::Finished`] or [`Message::Rejected`]. Dropping the
/// connection before that withdraws the request.
///
/// # Errors
/// When the connection fails or the daemon breaks the protocol.
pub fn build(
    mut stream: UnixStream,
    request: BuildRequest,
    mut on_message: impl FnMut(&Message),
) -> Result<Message, String> {
    send(&mut stream, &Request::Build(request))?;
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|error| format!("lost the daemon: {error}"))?;
        let message = serde_json::from_str::<Message>(&line)
            .map_err(|error| format!("the daemon sent an invalid message: {error}"))?;
        on_message(&message);
        if message.is_final() {
            return Ok(message);
        }
    }
    Err("the daemon closed the connection before the build ended".into())
}

/// The daemon's slots and queue.
///
/// # Errors
/// When the connection fails or the daemon breaks the protocol.
pub fn status(stream: UnixStream) -> Result<Status, String> {
    ask(stream, &Request::Status)
}

/// A cache request did not produce a service-issued response. None of these
/// errors establishes whether an earlier cleanup executed; only its exact
/// receipt can settle that question.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "reason", rename_all = "snake_case")]
pub enum CacheError {
    Transport(String),
    /// Includes a daemon that does not understand cache requests. Preserve its
    /// rejection instead of treating it as malformed cache data.
    Rejected(String),
    InvalidResponse(String),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(reason) => write!(formatter, "cache transport failed: {reason}"),
            Self::Rejected(reason) => write!(formatter, "cache request rejected: {reason}"),
            Self::InvalidResponse(reason) => write!(formatter, "invalid cache response: {reason}"),
        }
    }
}

impl std::error::Error for CacheError {}

// The owner's bounded inventory is far smaller than this. Cap allocation
// before decoding an external daemon's answer, including rejection messages.
pub(crate) const MAX_CACHE_RESPONSE_BYTES: usize = 1024 * 1024;

/// Ask the cache owner; never retry a transport failure as a new cleanup.
/// Query the exact preview again to recover its receipt while it is live.
/// The caller owns the connection's read and write deadlines.
///
/// # Errors
/// A daemon rejection, transport failure or invalid response. A daemon that
/// lacks the cache protocol rejects the capability request; no inventory is
/// fabricated from its status or the filesystem.
pub fn cache(
    mut stream: UnixStream,
    host: Option<String>,
    operation: crate::cache::Operation,
) -> Result<crate::cache::Response, CacheError> {
    send(&mut stream, &Request::Cache { host, operation }).map_err(CacheError::Transport)?;
    read_cache_response(BufReader::new(stream), MAX_CACHE_RESPONSE_BYTES)
}

pub(crate) fn read_cache_response(
    reader: impl std::io::BufRead,
    limit: usize,
) -> Result<crate::cache::Response, CacheError> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(|error| CacheError::Transport(error.to_string()))?;
    if bytes.len() > limit {
        return Err(CacheError::InvalidResponse(format!(
            "answer exceeds {limit} bytes"
        )));
    }
    if !bytes.ends_with(b"\n") {
        return Err(CacheError::Transport(
            "the daemon closed the connection before its answer ended".into(),
        ));
    }
    decode_cache_response(&bytes)
}

pub(crate) fn decode_cache_response(bytes: &[u8]) -> Result<crate::cache::Response, CacheError> {
    serde_json::from_slice::<Result<crate::cache::Response, CacheError>>(bytes).map_err(
        |error| {
            if let Ok(Message::Rejected { reason }) = serde_json::from_slice(bytes) {
                CacheError::Rejected(reason)
            } else {
                CacheError::InvalidResponse(error.to_string())
            }
        },
    )?
}

/// The daemon's slots and queue, its totals since it started, and its
/// recent events.
///
/// # Errors
/// When the connection fails or the daemon breaks the protocol.
pub fn activity(stream: UnixStream) -> Result<Activity, String> {
    ask(stream, &Request::Activity)
}

/// The builds that failed in the last `hours`, on the remote host for `os`
/// when that is not this machine's.
///
/// # Errors
/// When the connection fails, the daemon breaks the protocol, or no host
/// builds for `os` or it could not be asked.
pub fn failures(stream: UnixStream, hours: u64, os: Option<String>) -> Result<Failures, String> {
    ask::<Result<Failures, String>>(stream, &Request::Failures { hours, os })?
}

/// Stops the daemon taking new builds (`drain`), or makes it take them again.
///
/// # Errors
/// When the connection fails or the daemon breaks the protocol.
pub fn drain(stream: UnixStream, drain: bool) -> Result<Status, String> {
    ask(stream, &Request::Drain { drain })
}

/// Sends `request` and reads its one-line answer.
fn ask<T: DeserializeOwned>(mut stream: UnixStream, request: &Request) -> Result<T, String> {
    send(&mut stream, request)?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| format!("lost the daemon: {error}"))?;
    serde_json::from_str(&line)
        .map_err(|error| format!("the daemon sent an invalid answer: {error}"))
}

fn send(stream: &mut UnixStream, request: &Request) -> Result<(), String> {
    let mut text = serde_json::to_string(request).expect("requests serialize");
    text.push('\n');
    stream
        .write_all(text.as_bytes())
        .map_err(|error| format!("could not send the request: {error}"))
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::cache::{Refusal, Response};

    fn answer(bytes: &[u8], limit: usize) -> Result<Response, CacheError> {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(bytes).unwrap();
        drop(writer);
        read_cache_response(BufReader::new(reader), limit)
    }

    #[test]
    fn daemon_rejection_is_distinct_from_transport_and_invalid_data() {
        let rejection = b"{\"type\":\"rejected\",\"reason\":\"unknown request cache\"}\n";
        assert_eq!(
            answer(rejection, 4096),
            Err(CacheError::Rejected("unknown request cache".into()))
        );
        assert!(matches!(answer(b"", 4096), Err(CacheError::Transport(_))));
        assert!(matches!(
            answer(b"{\"type\":\"refused\"", 4096),
            Err(CacheError::Transport(_))
        ));
        for bytes in [b"{}\n".as_slice(), b"\xff\n", b"{\"type\":\"rejected\"}\n"] {
            assert!(matches!(
                answer(bytes, 4096),
                Err(CacheError::InvalidResponse(_))
            ));
        }
        assert_eq!(
            answer(
                b"{\"Ok\":{\"type\":\"refused\",\"reason\":\"busy\"}}\n",
                4096
            ),
            Ok(Response::Refused {
                reason: Refusal::Busy
            })
        );
    }

    #[test]
    fn forwarded_errors_keep_their_wire_types() {
        for error in [
            CacheError::Transport("lost remote after execute".into()),
            CacheError::Rejected("unsupported cache request".into()),
            CacheError::InvalidResponse("remote sent malformed data".into()),
        ] {
            let response: Result<Response, CacheError> = Err(error.clone());
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            assert_eq!(answer(&bytes, 4096), Err(error));
        }
    }

    #[test]
    fn cache_answers_are_bounded_before_decoding() {
        let mut bytes = vec![b' '; 4096];
        bytes.push(b'\n');
        assert_eq!(
            answer(&bytes, 4096),
            Err(CacheError::InvalidResponse(
                "answer exceeds 4096 bytes".into()
            ))
        );
        let bytes = b"{\"Ok\":{\"type\":\"refused\",\"reason\":\"busy\"}}\n";
        assert!(answer(bytes, bytes.len()).is_ok());
        assert!(matches!(
            answer(bytes, bytes.len() - 1),
            Err(CacheError::InvalidResponse(_))
        ));
    }
}
