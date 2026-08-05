use std::{io, time::Duration};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const DS4D_MAGIC: u32 = 0x4453_3444;
pub const DS4D_HELLO_KIND: u32 = 1;
pub const HELLO_FIXED_BYTES: usize = 40;
pub const HELLO_MAX_MODEL_NAME_BYTES: usize = 127;
const HEADER_BYTES: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ds4Hello {
    pub model_id: u32,
    pub quant_bits: u32,
    pub layer_start: u32,
    pub layer_end: u32,
    pub has_output: bool,
    pub has_hidden: bool,
    pub context_size: u32,
    pub layer_count: u32,
    pub listen_port: u16,
    pub model_name: String,
}

#[derive(Debug, Error)]
pub enum Ds4HelloError {
    #[error("DS4 HELLO read timed out")]
    Timeout,
    #[error("DS4 frame is truncated")]
    Truncated,
    #[error("invalid DS4 frame magic")]
    InvalidMagic,
    #[error("unsupported DS4 frame kind")]
    InvalidKind,
    #[error("invalid DS4 HELLO payload size")]
    InvalidSize,
    #[error("DS4 HELLO model name is too long")]
    ModelNameTooLong,
    #[error("DS4 HELLO model name is not valid UTF-8")]
    InvalidModelName,
    #[error("DS4 HELLO contains an invalid fixed field")]
    InvalidField,
    #[error("DS4 HELLO contains trailing data")]
    TrailingData,
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub fn parse_hello_frame(frame: &[u8]) -> Result<Ds4Hello, Ds4HelloError> {
    if frame.len() < HEADER_BYTES {
        return Err(Ds4HelloError::Truncated);
    }
    let magic = read_u32(frame, 0);
    let kind = read_u32(frame, 4);
    let payload_bytes = read_u32(frame, 8) as usize;
    if magic != DS4D_MAGIC {
        return Err(Ds4HelloError::InvalidMagic);
    }
    if kind != DS4D_HELLO_KIND {
        return Err(Ds4HelloError::InvalidKind);
    }
    if !(HELLO_FIXED_BYTES..=HELLO_FIXED_BYTES + HELLO_MAX_MODEL_NAME_BYTES)
        .contains(&payload_bytes)
    {
        return Err(Ds4HelloError::InvalidSize);
    }
    let expected = HEADER_BYTES + payload_bytes;
    if frame.len() < expected {
        return Err(Ds4HelloError::Truncated);
    }
    if frame.len() > expected {
        return Err(Ds4HelloError::TrailingData);
    }
    let fixed = &frame[HEADER_BYTES..HEADER_BYTES + HELLO_FIXED_BYTES];
    let model_name_len = read_u32(fixed, 36) as usize;
    if model_name_len > HELLO_MAX_MODEL_NAME_BYTES {
        return Err(Ds4HelloError::ModelNameTooLong);
    }
    if payload_bytes != HELLO_FIXED_BYTES + model_name_len {
        return Err(Ds4HelloError::InvalidSize);
    }
    let bool_field = |offset| match read_u32(fixed, offset) {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Ds4HelloError::InvalidField),
    };
    let layer_start = read_u32(fixed, 8);
    let layer_end = read_u32(fixed, 12);
    let context_size = read_u32(fixed, 24);
    let layer_count = read_u32(fixed, 28);
    let listen_port = read_u32(fixed, 32);
    if layer_start > layer_end
        || context_size == 0
        || layer_count == 0
        || listen_port == 0
        || listen_port > u16::MAX as u32
        || model_name_len == 0
    {
        return Err(Ds4HelloError::InvalidField);
    }
    let model_name = std::str::from_utf8(&frame[HEADER_BYTES + HELLO_FIXED_BYTES..])
        .map_err(|_| Ds4HelloError::InvalidModelName)?
        .to_owned();
    Ok(Ds4Hello {
        model_id: read_u32(fixed, 0),
        // Kept for diagnostics only; acceptance must not identify MXFP4 from this field.
        quant_bits: read_u32(fixed, 4),
        layer_start,
        layer_end,
        has_output: bool_field(16)?,
        has_hidden: bool_field(20)?,
        context_size,
        layer_count,
        listen_port: listen_port as u16,
        model_name,
    })
}

pub async fn read_hello_frame<R>(
    reader: &mut R,
    deadline: Duration,
) -> Result<Ds4Hello, Ds4HelloError>
where
    R: AsyncRead + Unpin,
{
    if deadline.is_zero() {
        return Err(Ds4HelloError::Timeout);
    }
    tokio::time::timeout(deadline, async {
        let mut header = [0_u8; HEADER_BYTES];
        read_exact_or_truncated(reader, &mut header).await?;
        let payload_bytes = read_u32(&header, 8) as usize;
        if !(HELLO_FIXED_BYTES..=HELLO_FIXED_BYTES + HELLO_MAX_MODEL_NAME_BYTES)
            .contains(&payload_bytes)
        {
            return Err(Ds4HelloError::InvalidSize);
        }
        let mut frame = Vec::with_capacity(HEADER_BYTES + payload_bytes);
        frame.extend_from_slice(&header);
        frame.resize(HEADER_BYTES + payload_bytes, 0);
        read_exact_or_truncated(reader, &mut frame[HEADER_BYTES..]).await?;

        let mut trailing = [0_u8; 1];
        match tokio::time::timeout(Duration::from_millis(1), reader.read(&mut trailing)).await {
            Ok(Ok(0)) | Err(_) => {}
            Ok(Ok(_)) => return Err(Ds4HelloError::TrailingData),
            Ok(Err(error)) => return Err(Ds4HelloError::Io(error)),
        }
        parse_hello_frame(&frame)
    })
    .await
    .map_err(|_| Ds4HelloError::Timeout)?
}

async fn read_exact_or_truncated<R>(reader: &mut R, buffer: &mut [u8]) -> Result<(), Ds4HelloError>
where
    R: AsyncRead + Unpin,
{
    reader
        .read_exact(buffer)
        .await
        .map(|_| ())
        .map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                Ds4HelloError::Truncated
            } else {
                Ds4HelloError::Io(error)
            }
        })
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn fixture() -> Vec<u8> {
        include_str!("../../tests/fixtures/ds4/hello40-schema-v1.hex")
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .flat_map(|line| line.split_ascii_whitespace())
            .map(|value| u8::from_str_radix(value, 16).unwrap())
            .collect()
    }

    #[test]
    fn parses_known_network_order_fixture() {
        let hello = parse_hello_frame(&fixture()).unwrap();
        assert_eq!(hello.model_id, 1);
        assert_eq!(hello.quant_bits, 2);
        assert_eq!((hello.layer_start, hello.layer_end), (20, 42));
        assert!(hello.has_output && hello.has_hidden);
        assert_eq!(hello.context_size, 262_144);
        assert_eq!(hello.layer_count, 43);
        assert_eq!(hello.listen_port, 8000);
        assert_eq!(hello.model_name, "deepseek-v4-flash");
    }

    #[test]
    fn rejects_magic_kind_size_name_length_truncation_and_trailing_data() {
        for (offset, value, expected) in [
            (0, 0_u32, "magic"),
            (4, 2, "kind"),
            (8, 39, "size"),
            (48, 128, "name"),
        ] {
            let mut frame = fixture();
            frame[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
            let error = parse_hello_frame(&frame).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
        let mut truncated = fixture();
        truncated.pop();
        assert!(matches!(
            parse_hello_frame(&truncated),
            Err(Ds4HelloError::Truncated)
        ));
        let mut trailing = fixture();
        trailing.push(0);
        assert!(matches!(
            parse_hello_frame(&trailing),
            Err(Ds4HelloError::TrailingData)
        ));
    }

    #[tokio::test]
    async fn reader_enforces_deadline_and_trailing_data() {
        let (mut writer, mut reader) = tokio::io::duplex(256);
        let task = tokio::spawn(async move {
            writer.write_all(&fixture()).await.unwrap();
        });
        assert!(
            read_hello_frame(&mut reader, Duration::from_secs(1))
                .await
                .is_ok()
        );
        task.await.unwrap();

        let mut empty = tokio::io::empty();
        assert!(matches!(
            read_hello_frame(&mut empty, Duration::from_millis(10)).await,
            Err(Ds4HelloError::Truncated)
        ));
        let (_pending_writer, mut pending_reader) = tokio::io::duplex(16);
        assert!(matches!(
            read_hello_frame(&mut pending_reader, Duration::from_millis(10)).await,
            Err(Ds4HelloError::Timeout)
        ));
        let mut with_trailing = fixture();
        with_trailing.push(1);
        assert!(matches!(
            read_hello_frame(
                &mut std::io::Cursor::new(with_trailing),
                Duration::from_secs(1)
            )
            .await,
            Err(Ds4HelloError::TrailingData)
        ));
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut seed = 0x1234_5678_u32;
        for length in 0..512 {
            let mut bytes = vec![0_u8; length];
            for byte in &mut bytes {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *byte = (seed >> 24) as u8;
            }
            let _ = parse_hello_frame(&bytes);
        }
    }
}
