//! Keeps the application's versioned counter format separate from file persistence.

use rca_persistence::Codec;
use std::io;

/// Encodes exactly one versioned little-endian counter record.
///
/// The record is `RCACTR01` followed by eight value bytes. Decoding rejects
/// unknown versions, truncation and trailing bytes with `InvalidData`.
#[derive(Clone, Copy, Debug)]
pub struct CounterCodec;

impl Codec<u64> for CounterCodec {
    type Error = io::Error;

    fn encode(&self, value: &u64) -> Result<Vec<u8>, io::Error> {
        // Reserve the complete record before writing either field.
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(b"RCACTR01");
        bytes.extend_from_slice(&value.to_le_bytes());
        Ok(bytes)
    }

    fn decode(&self, bytes: &[u8]) -> Result<u64, io::Error> {
        if bytes.len() != 16 || &bytes[..8] != b"RCACTR01" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid counter record",
            ));
        }
        let mut value = [0; 8];
        value.copy_from_slice(&bytes[8..]);
        Ok(u64::from_le_bytes(value))
    }
}
