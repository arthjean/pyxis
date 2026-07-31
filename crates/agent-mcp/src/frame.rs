//! Frame bound shared by both transports.
//!
//! Every other bound in this crate (listing pages, item counts, cursor sizes,
//! result text, image budgets) sits ABOVE the point where a message has already
//! been read into memory. Under all of them, both SDK paths read a frame with no
//! ceiling at all:
//!
//! - stdio: `rmcp`'s `AsyncRwTransport` does `BufReader::read_until(b'\n')` into a
//!   `Vec` it never caps, so a server that writes a gigabyte without a newline
//!   makes the client allocate a gigabyte.
//! - Streamable HTTP: an SSE body is decoded event by event with no cap either.
//!
//! Both are line-oriented, so one adapter covers both: count the bytes since the
//! last newline and fail the stream past the cap. `rmcp` turns a read error into
//! an end of stream, so an oversized frame disconnects the server rather than
//! feeding it.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, ReadBuf};

/// Max bytes of one JSON-RPC frame accepted from a server. Two orders of
/// magnitude above any legitimate message (a tool result is capped at 30 KB
/// before it reaches the model) and small enough that a hostile server cannot
/// spend the process's memory on a single line.
pub(crate) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Counts the bytes of the frame currently being read. Shared by the two
/// adapters so the accounting rule exists once.
#[derive(Debug, Default)]
pub(crate) struct FrameCounter {
    since_newline: usize,
}

impl FrameCounter {
    /// Accounts for a freshly read slice. `Err` = the frame in flight passed the
    /// cap and the stream must end.
    pub(crate) fn accept(&mut self, fresh: &[u8], max: usize) -> io::Result<()> {
        // Only the run AFTER the last newline can still grow: everything before
        // it is a complete frame the parser is about to take.
        self.since_newline = match fresh.iter().rposition(|byte| *byte == b'\n') {
            Some(last) => fresh.len() - last - 1,
            None => self.since_newline.saturating_add(fresh.len()),
        };
        if self.since_newline > max {
            return Err(io::Error::other(format!(
                "MCP frame larger than {max} bytes"
            )));
        }
        Ok(())
    }
}

/// `AsyncRead` refusing a frame larger than `max`.
pub(crate) struct BoundedFrames<R> {
    inner: R,
    counter: FrameCounter,
    max: usize,
}

impl<R> BoundedFrames<R> {
    pub(crate) fn new(inner: R, max: usize) -> Self {
        Self {
            inner,
            counter: FrameCounter::default(),
            max,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedFrames<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let before = buf.filled().len();
        ready!(Pin::new(&mut me.inner).poll_read(cx, buf))?;
        let fresh = &buf.filled()[before..];
        match me.counter.accept(fresh, me.max) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(err) => {
                // `AsyncRead` forbids reporting an error and having filled the
                // buffer in the same poll (tokio asserts on it). Rewinding to
                // where this poll started drops the offending bytes, which is
                // what we want anyway: nothing of an oversized frame travels.
                buf.set_filled(before);
                Poll::Ready(Err(err))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn a_frame_within_the_cap_travels_whole() {
        let body = format!("{}\n{}\n", "a".repeat(4_000), "b".repeat(4_000));
        let mut reader = BoundedFrames::new(std::io::Cursor::new(body.clone().into_bytes()), 8_192);
        let mut read = String::new();
        reader.read_to_string(&mut read).await.expect("within cap");
        assert_eq!(read, body);
    }

    /// The counter resets on every newline, so a long stream of normal frames is
    /// never mistaken for one oversized frame.
    #[tokio::test]
    async fn many_frames_never_add_up_to_the_cap() {
        let body = "x".repeat(100) + "\n";
        let stream = body.repeat(500);
        let mut reader = BoundedFrames::new(std::io::Cursor::new(stream.clone().into_bytes()), 256);
        let mut read = String::new();
        reader.read_to_string(&mut read).await.expect("within cap");
        assert_eq!(read.len(), stream.len());
    }

    /// The whole point: a server writing without a newline is cut off instead of
    /// being allowed to allocate without end.
    #[tokio::test]
    async fn an_unterminated_frame_ends_the_stream() {
        let body = "x".repeat(10_000);
        let mut reader = BoundedFrames::new(std::io::Cursor::new(body.into_bytes()), 1_024);
        let mut read = Vec::new();
        let err = reader
            .read_to_end(&mut read)
            .await
            .expect_err("an oversized frame must fail the stream");
        assert!(err.to_string().contains("larger than 1024"), "{err}");
    }

    #[test]
    fn the_count_restarts_at_each_newline() {
        let mut counter = FrameCounter::default();
        counter.accept(b"abc", 4).expect("under the cap");
        // Would be 6 bytes without the reset, which is over the cap.
        counter.accept(b"d\nef", 4).expect("the newline resets");
        assert!(counter.accept(b"ghi", 4).is_err());
    }
}
