//! The outbound writer loop both connection ends share.
//!
//! Every frame a connection sends funnels through one mpsc channel into a
//! task that owns the write half of the stream. That task used to `send()`
//! each frame — encode + flush, so one syscall and typically one TCP segment
//! per frame. Wrong shape for pipelined traffic: a ReadMany response stream,
//! a 32-deep readahead window, or a Tree page train queues many frames at
//! once, and flushing between frames that are already waiting buys nothing
//! but syscalls. So the loop now feeds frames into the codec buffer while
//! more are already queued, and flushes once when the queue goes quiet.

use futures::SinkExt;
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;
use tokio_util::codec::FramedWrite;

use alloyfs_proto::{Frame, FrameCodec};

/// Flush at least every this-many fed frames even when the queue never
/// drains. "Flush when empty" alone has a failure mode: a sender that keeps
/// the queue topped up would defer the flush forever, and frames this side
/// considers sent would sit in the codec buffer while the peer starves (the
/// sink does spill to the socket on its own once the buffer grows, but that
/// is a tokio-util implementation detail, not a latency promise). A batch of
/// 32 bounds how long any frame can wait behind the ones fed after it, while
/// still collapsing a burst into ~1/32 of the flushes.
const MAX_UNFLUSHED: usize = 32;

/// Drain `rx` into `writer` until the channel closes or a write fails.
///
/// Greedy: after waking for one frame, everything already queued is fed too,
/// then the stream is flushed once. A lone frame — a Ping, a keepalive Pong —
/// finds the queue empty behind it and is flushed immediately, so liveness
/// traffic never waits on batching.
///
/// Any encode or write error ends the loop, exactly as it ended the old
/// per-frame `send()` loop: the task dies, the socket half drops, and the
/// peer's read side notices. Nothing here retries or degrades.
pub(crate) async fn drain<W>(mut writer: FramedWrite<W, FrameCodec>, mut rx: mpsc::Receiver<Frame>)
where
    W: AsyncWrite + Unpin,
{
    'conn: while let Some(frame) = rx.recv().await {
        if writer.feed(&frame).await.is_err() {
            break;
        }
        let mut unflushed = 1usize;
        while let Ok(frame) = rx.try_recv() {
            if unflushed >= MAX_UNFLUSHED {
                if writer.flush().await.is_err() {
                    break 'conn;
                }
                unflushed = 0;
            }
            if writer.feed(&frame).await.is_err() {
                break 'conn;
            }
            unflushed += 1;
        }
        // The queue went quiet (or closed — try_recv drains buffered frames
        // first, so nothing is lost): everything fed so far goes out now.
        if writer.flush().await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use futures::StreamExt;
    use tokio_util::codec::FramedRead;

    use super::*;

    /// Counts completed flushes on the wrapped writer. The flush count is the
    /// one thing batching changes that the wire cannot show (the same bytes
    /// arrive either way), and the `AsyncWrite` seam measures it without any
    /// hook inside the loop itself.
    struct CountFlushes<W> {
        inner: W,
        flushes: Arc<AtomicUsize>,
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for CountFlushes<W> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            let ready = Pin::new(&mut self.inner).poll_flush(cx);
            if matches!(ready, Poll::Ready(Ok(()))) {
                self.flushes.fetch_add(1, Ordering::SeqCst);
            }
            ready
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// A pre-queued burst must arrive complete and in order after far fewer
    /// flushes than frames. Pre-loading the channel before the loop starts
    /// makes the drain deterministic: N frames, batches of MAX_UNFLUSHED,
    /// plus the final flush when the queue empties.
    #[tokio::test]
    async fn a_burst_is_flushed_in_batches_not_per_frame() {
        const N: usize = 100;
        let (w, r) = tokio::io::duplex(1024 * 1024);
        let flushes = Arc::new(AtomicUsize::new(0));
        let writer = FramedWrite::new(
            CountFlushes {
                inner: w,
                flushes: flushes.clone(),
            },
            FrameCodec::default(),
        );

        let (tx, rx) = mpsc::channel::<Frame>(N);
        for nonce in 0..N as u64 {
            tx.try_send(Frame::Ping { nonce })
                .expect("pre-load fits the channel");
        }
        drop(tx); // drain everything, then end on the closed channel

        drain(writer, rx).await;

        let mut reader = FramedRead::new(r, FrameCodec::default());
        for want in 0..N as u64 {
            match reader.next().await {
                Some(Ok(Frame::Ping { nonce })) => {
                    assert_eq!(nonce, want, "batching must not reorder frames")
                }
                other => panic!("frame {want}: expected Ping, got {other:?}"),
            }
        }

        let flushed = flushes.load(Ordering::SeqCst);
        assert!(flushed >= 1, "the burst has to be flushed at all");
        assert!(
            flushed <= N / MAX_UNFLUSHED + 1,
            "{flushed} flushes for {N} pre-queued frames — the burst was flushed per frame, not per batch"
        );
    }

    /// A frame with nothing queued behind it flushes immediately — the
    /// keepalive path (Ping/Pong) must never sit in the codec buffer waiting
    /// for company that may not come.
    #[tokio::test]
    async fn a_lone_frame_is_flushed_immediately() {
        let (w, r) = tokio::io::duplex(64 * 1024);
        let writer = FramedWrite::new(w, FrameCodec::default());
        let (tx, rx) = mpsc::channel::<Frame>(8);
        let task = tokio::spawn(drain(writer, rx));

        let mut reader = FramedRead::new(r, FrameCodec::default());
        tx.send(Frame::Ping { nonce: 7 }).await.expect("writer alive");
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), reader.next())
            .await
            .expect("a lone frame must go out without waiting for more");
        assert!(matches!(got, Some(Ok(Frame::Ping { nonce: 7 }))));

        drop(tx);
        task.await.expect("writer task ends when the channel closes");
    }
}
