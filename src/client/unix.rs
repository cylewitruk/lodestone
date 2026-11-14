//! A ureq transport over the daemon's Unix socket: every request goes to the socket, whatever
//! host its URL names.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
};
use ureq::{Error, Timeout};

#[derive(Debug)]
pub struct UnixConnector(pub PathBuf);

impl Connector for UnixConnector {
    type Out = UnixTransport;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _chained: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        let config = details.config;
        Ok(Some(UnixTransport {
            stream: UnixStream::connect(&self.0)?,
            buffers: LazyBuffers::new(config.input_buffer_size(), config.output_buffer_size()),
        }))
    }
}

pub struct UnixTransport {
    stream: UnixStream,
    buffers: LazyBuffers,
}

impl fmt::Debug for UnixTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnixTransport").finish_non_exhaustive()
    }
}

/// Maps an I/O error to ureq's, reporting a socket timeout as the timeout that applied.
fn io_error(e: io::Error, reason: Timeout) -> Error {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Error::Timeout(reason),
        _ => e.into(),
    }
}

/// The result of setting a socket timeout. macOS refuses (`EINVAL`) once the peer has closed,
/// when a read returns what the peer sent, or the end of the stream, without waiting.
fn timeout_set(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if cfg!(target_os = "macos") && e.kind() == io::ErrorKind::InvalidInput => Ok(()),
        r => r,
    }
}

impl Transport for UnixTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        let timeout = timeout.for_write();
        timeout_set(
            self.stream
                .set_write_timeout(timeout.not_zero().map(|t| *t)),
        )?;
        self.stream
            .write_all(&self.buffers.output()[..amount])
            .map_err(|e| io_error(e, timeout.reason))
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        let timeout = timeout.for_read();
        timeout_set(self.stream.set_read_timeout(timeout.not_zero().map(|t| *t)))?;
        let input = self.buffers.input_append_buf();
        let amount = loop {
            match self.stream.read(input) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => break r.map_err(|e| io_error(e, timeout.reason))?,
            }
        };
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    /// Open when nothing is waiting to be read and the peer has not closed (pooled connections).
    fn is_open(&mut self) -> bool {
        if self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let open = matches!(
            self.stream.read(&mut [0]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
        );
        self.stream.set_nonblocking(false).is_ok() && open
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_response_is_read_after_the_peer_closed() {
        let (mut ours, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(b"x").unwrap();
        drop(peer);
        timeout_set(ours.set_read_timeout(Some(Duration::from_secs(1)))).unwrap();
        let mut buf = [0; 2];
        assert_eq!(ours.read(&mut buf).unwrap(), 1);
    }
}
