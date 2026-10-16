use crate::MAX_FRAME;
use serde::{Serialize, de::DeserializeOwned};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid or oversized IPC frame")
}

pub async fn read<T: DeserializeOwned>(stream: &mut (impl AsyncRead + Unpin)) -> io::Result<T> {
    let len = stream.read_u32().await? as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(invalid());
    }
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}

pub async fn write<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|_| invalid())?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(invalid());
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn oversized_frames_are_rejected_before_reading_payload() {
        let data = ((MAX_FRAME + 1) as u32).to_be_bytes();
        assert_eq!(
            read::<serde_json::Value>(&mut data.as_slice())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[tokio::test]
    async fn malformed_and_truncated_frames_are_rejected() {
        let mut malformed: &[u8] = b"\0\0\0\x03bad";
        assert!(read::<serde_json::Value>(&mut malformed).await.is_err());
        let mut truncated: &[u8] = b"\0\0\0\x05{}";
        assert!(read::<serde_json::Value>(&mut truncated).await.is_err());
    }
}
