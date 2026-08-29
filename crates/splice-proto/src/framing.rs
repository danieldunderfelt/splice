//! Length-prefixed frame codec over any `AsyncRead`/`AsyncWrite`.
//!
//! Wire format: `u32` big-endian payload length (≤ [`crate::MAX_FRAME_LEN`]) followed by the
//! postcard-encoded [`crate::Frame`]. The writer buffers one frame and flushes per send —
//! callers set `TCP_NODELAY` on the socket.

use crate::{Frame, ProtoError, MAX_FRAME_LEN};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

struct ReuseBuffer<'a>(&'a mut Vec<u8>);

impl Extend<u8> for ReuseBuffer<'_> {
    fn extend<T: IntoIterator<Item = u8>>(&mut self, iter: T) {
        self.0.extend(iter);
    }
}

/// Read one frame. Cancel-safe ONLY at the length-prefix boundary; callers should own the
/// read half exclusively in a dedicated task.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame, ProtoError> {
    let mut buf = Vec::new();
    read_frame_buffered(r, &mut buf).await
}

pub async fn read_frame_buffered<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Frame, ProtoError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(ProtoError::FrameTooLarge(len));
    }
    buf.resize(len as usize, 0);
    r.read_exact(buf).await?;
    Ok(postcard::from_bytes(buf)?)
}

/// Write one frame and flush.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> Result<(), ProtoError> {
    let mut buf = Vec::new();
    write_frame_buffered(w, frame, &mut buf).await
}

pub async fn write_frame_buffered<W: AsyncWrite + Unpin>(
    w: &mut W,
    frame: &Frame,
    buf: &mut Vec<u8>,
) -> Result<(), ProtoError> {
    buf.clear();
    buf.resize(4, 0);
    postcard::to_extend(frame, ReuseBuffer(buf))?;
    let len = buf.len() - 4;
    if len > MAX_FRAME_LEN as usize {
        return Err(ProtoError::FrameTooLarge(len.min(u32::MAX as usize) as u32));
    }
    buf[..4].copy_from_slice(&(len as u32).to_be_bytes());
    w.write_all(buf).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InputEvent, PROTO_VERSION};

    #[tokio::test]
    async fn roundtrip_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let f = Frame::Input {
            session: 1,
            ev: InputEvent::Key { code: 30, pressed: true },
        };
        write_frame(&mut a, &f).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(f, got);
        assert_eq!(PROTO_VERSION, 1);
    }

    #[tokio::test]
    async fn oversized_frame_rejected() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // Hand-craft a bogus length prefix.
        tokio::io::AsyncWriteExt::write_all(&mut a, &(MAX_FRAME_LEN + 1).to_be_bytes())
            .await
            .unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, ProtoError::FrameTooLarge(_)));
    }
}
