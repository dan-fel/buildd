//! The stream the daemon and its clients talk over: a Unix domain socket,
//! which Windows provides through Winsock.

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
pub use uds_windows::{UnixListener, UnixStream};
