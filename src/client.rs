//! Talking to a running daemon.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::config;
use crate::protocol::{BuildRequest, Message, Request, Status};

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
pub fn status(mut stream: UnixStream) -> Result<Status, String> {
    send(&mut stream, &Request::Status)?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| format!("lost the daemon: {error}"))?;
    serde_json::from_str(&line)
        .map_err(|error| format!("the daemon sent an invalid status: {error}"))
}

fn send(stream: &mut UnixStream, request: &Request) -> Result<(), String> {
    let mut text = serde_json::to_string(request).expect("requests serialize");
    text.push('\n');
    stream
        .write_all(text.as_bytes())
        .map_err(|error| format!("could not send the request: {error}"))
}
