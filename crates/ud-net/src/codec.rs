//! Length prefixed records.
//!
//! Wire format for a single record is a little endian `u32` length followed by
//! that many bytes. During the handshake those bytes are a raw Noise message;
//! afterwards they are a Noise ciphertext whose plaintext starts with a one byte
//! inner tag that separates JSON envelopes from raw file payloads.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use ud_core::protocol::{FileChunk, Message};

use crate::error::{Error, Result};

/// Largest record we will accept, matching the Noise message ceiling.
pub const MAX_RECORD: usize = 65535;

pub const INNER_MESSAGE: u8 = 0;
pub const INNER_CHUNK: u8 = 1;

/// What a peer sent us.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Message(Message),
    Chunk(FileChunk),
}

/// What we send to a peer.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    Message(Message),
    Chunk(FileChunk),
}

impl Outgoing {
    pub fn is_urgent(&self) -> bool {
        matches!(self, Outgoing::Message(m) if m.is_urgent())
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Outgoing::Message(m) => m.kind(),
            Outgoing::Chunk(_) => "chunk",
        }
    }
}

/// Serializes an outgoing record into an inner plaintext buffer.
pub fn encode_inner(item: &Outgoing) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match item {
        Outgoing::Message(message) => {
            out.push(INNER_MESSAGE);
            let json = serde_json::to_vec(message)?;
            if json.len() + 1 > MAX_RECORD {
                return Err(Error::Range(format!(
                    "control message of {} bytes exceeds the record limit",
                    json.len()
                )));
            }
            out.extend_from_slice(&json);
        }
        Outgoing::Chunk(chunk) => {
            out.push(INNER_CHUNK);
            out.extend_from_slice(&chunk.encode());
        }
    }
    Ok(out)
}

/// Reverses [`encode_inner`].
pub fn decode_inner(plaintext: &[u8]) -> Result<Incoming> {
    let (tag, body) = plaintext
        .split_first()
        .ok_or_else(|| Error::protocol("empty record"))?;
    match *tag {
        INNER_MESSAGE => Ok(Incoming::Message(serde_json::from_slice(body)?)),
        INNER_CHUNK => Ok(Incoming::Chunk(FileChunk::decode(body).map_err(|e| {
            Error::protocol(e.to_string())
        })?)),
        other => Err(Error::protocol(format!("unknown record tag {other}"))),
    }
}

pub async fn write_record<W: AsyncWrite + Unpin>(writer: &mut W, body: &[u8]) -> Result<()> {
    if body.len() > MAX_RECORD {
        return Err(Error::Range(format!(
            "record of {} bytes exceeds the {} byte limit",
            body.len(),
            MAX_RECORD
        )));
    }
    let mut header = [0u8; 4];
    header.copy_from_slice(&(body.len() as u32).to_le_bytes());
    writer.write_all(&header).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one record into `buf`, reusing the allocation between calls.
pub async fn read_record<R: AsyncRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> Result<()> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).await?;
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_RECORD {
        return Err(Error::Range(format!("peer announced a {len} byte record")));
    }
    buf.clear();
    buf.resize(len, 0);
    reader.read_exact(buf).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ud_core::protocol::TransferId;

    #[tokio::test]
    async fn records_round_trip_through_a_pipe() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let chunk = Outgoing::Chunk(FileChunk {
            transfer: TransferId(1),
            index: 0,
            offset: 0,
            data: vec![9; 1024],
        });
        let encoded = encode_inner(&chunk).unwrap();
        let payload = encoded.clone();
        tokio::spawn(async move {
            write_record(&mut a, &payload).await.unwrap();
        });
        let mut buf = Vec::new();
        read_record(&mut b, &mut buf).await.unwrap();
        assert_eq!(buf, encoded);
        assert_eq!(decode_inner(&buf).unwrap(), Incoming::Chunk(match chunk {
            Outgoing::Chunk(c) => c,
            _ => unreachable!(),
        }));
    }

    #[tokio::test]
    async fn oversized_records_are_refused_before_they_hit_the_wire() {
        let mut sink = Vec::new();
        let err = write_record(&mut sink, &vec![0u8; MAX_RECORD + 1])
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Range(_)));
    }
}
