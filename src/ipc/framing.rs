use std::io;

use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Errors encountered while reading or writing bounded NDJSON frames.
#[derive(Debug, Error)]
pub enum FrameError {
    #[error("NDJSON line exceeds maximum frame limit of {max_bytes} bytes")]
    TooLarge { max_bytes: usize },
    #[error("NDJSON line is not valid UTF-8")]
    InvalidUtf8,
    #[error("JSON serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("socket I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Reads a single UTF-8 line capped at `max_bytes` without unbounded allocation.
///
/// Returns `Ok(None)` on clean EOF when no trailing bytes were read.
pub async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<String>, FrameError> {
    let mut buf = Vec::new();

    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }

        if let Some(newline_pos) = chunk.iter().position(|&b| b == b'\n') {
            if buf.len() + newline_pos > max_bytes {
                reader.consume(newline_pos + 1);
                return Err(FrameError::TooLarge { max_bytes });
            }
            buf.extend_from_slice(&chunk[..newline_pos]);
            reader.consume(newline_pos + 1);
            break;
        }

        if buf.len() + chunk.len() > max_bytes {
            let len = chunk.len();
            reader.consume(len);
            return Err(FrameError::TooLarge { max_bytes });
        }

        buf.extend_from_slice(chunk);
        let len = chunk.len();
        reader.consume(len);
    }

    if buf.last() == Some(&b'\r') {
        buf.pop();
    }

    let line = String::from_utf8(buf).map_err(|_| FrameError::InvalidUtf8)?;
    Ok(Some(line))
}

async fn write_ndjson_line_inner<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
    max_bytes: Option<usize>,
) -> Result<(), FrameError> {
    let mut bytes = serde_json::to_vec(value)?;
    if let Some(max) = max_bytes
        && bytes.len() > max
    {
        return Err(FrameError::TooLarge { max_bytes: max });
    }
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Serializes `value` as a single-line JSON payload followed by `\n` and flushes.
pub async fn write_ndjson_line<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), FrameError> {
    write_ndjson_line_inner(writer, value, None).await
}

/// Serializes `value` as a single-line JSON payload capped at `max_bytes` (excluding `\n`) and flushes.
///
/// Leaves `writer` untouched and returns `Err(FrameError::TooLarge { max_bytes })` if the
/// serialized payload exceeds `max_bytes`.
pub async fn write_bounded_ndjson_line<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
    max_bytes: usize,
) -> Result<(), FrameError> {
    write_ndjson_line_inner(writer, value, Some(max_bytes)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_bounded_ndjson_line_rejects_oversized_payload_without_writing() {
        let mut sink = Vec::new();
        let payload = serde_json::json!({ "pad": "x".repeat(128) });
        let err = write_bounded_ndjson_line(&mut sink, &payload, 64)
            .await
            .unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { max_bytes: 64 }));
        assert!(
            sink.is_empty(),
            "oversized payload must not partially write"
        );
    }
}
