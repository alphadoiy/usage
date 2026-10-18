//! Passive bounded frame decoding. Never changes bytes or negotiates extensions.
const MAX_MESSAGE: usize = 1024 * 1024;
#[derive(Default)]
pub(super) struct Frames {
    buffer: Vec<u8>,
    fragmented: Option<(Vec<u8>, bool)>,
    deflate: Option<Deflate>,
}
struct Deflate {
    decoder: flate2::Decompress,
    reset: bool,
}
impl Deflate {
    fn decode(&mut self, bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
        if self.reset {
            self.decoder.reset(false);
        }
        // RFC 7692 removes the sync-flush tail from each message on the wire.
        let mut input = bytes.to_vec();
        input.extend_from_slice(&[0, 0, 255, 255]);
        let mut position = 0;
        let mut output = Vec::new();
        loop {
            let mut chunk = [0; 8192];
            let before_in = self.decoder.total_in();
            let before_out = self.decoder.total_out();
            let status = self
                .decoder
                .decompress(
                    &input[position..],
                    &mut chunk,
                    flate2::FlushDecompress::Sync,
                )
                .map_err(|_| "websocket_invalid_deflate")?;
            let consumed = (self.decoder.total_in() - before_in) as usize;
            let produced = (self.decoder.total_out() - before_out) as usize;
            if output.len() + produced > MAX_MESSAGE {
                return Err("websocket_decompressed_message_limit");
            }
            output.extend_from_slice(&chunk[..produced]);
            position += consumed;
            if position == input.len() && produced < chunk.len() {
                return Ok(output);
            }
            if status == flate2::Status::StreamEnd || (consumed == 0 && produced == 0) {
                return Err("websocket_invalid_deflate");
            }
        }
    }
}
impl Frames {
    pub fn negotiated(extensions: Option<&str>, client: bool) -> Self {
        let Some(extension) = extensions else {
            return Self::default();
        };
        let mut params = extension.split(';').map(str::trim);
        if params.next() != Some("permessage-deflate") || extension.contains(',') {
            return Self::default();
        }
        let mut reset = false;
        for param in params {
            match param {
                "client_no_context_takeover" => {
                    if client {
                        reset = true;
                    }
                }
                "server_no_context_takeover" => {
                    if !client {
                        reset = true;
                    }
                }
                p if p.split_once('=').is_some_and(|(key, value)| {
                    matches!(
                        key.trim(),
                        "client_max_window_bits" | "server_max_window_bits"
                    ) && value
                        .trim()
                        .trim_matches('"')
                        .parse::<u8>()
                        .is_ok_and(|v| (8..=15).contains(&v))
                }) => {}
                _ => return Self::default(),
            }
        }
        Self {
            deflate: Some(Deflate {
                decoder: flate2::Decompress::new(false),
                reset,
            }),
            ..Self::default()
        }
    }
    fn emit(
        &mut self,
        bytes: &[u8],
        compressed: bool,
        emit: &mut impl FnMut(&[u8]),
    ) -> Option<&'static str> {
        if compressed {
            let Some(deflate) = self.deflate.as_mut() else {
                return Some("websocket_extension_encoding");
            };
            match deflate.decode(bytes) {
                Ok(decoded) => emit(&decoded),
                Err(reason) => return Some(reason),
            }
        } else {
            emit(bytes);
        }
        None
    }
    pub fn feed(
        &mut self,
        bytes: &[u8],
        client: bool,
        mut emit: impl FnMut(&[u8]),
    ) -> Option<&'static str> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_MESSAGE + 14 {
            return Some("websocket_message_limit");
        }
        self.buffer.extend_from_slice(bytes);
        let mut consumed = 0;
        loop {
            let bytes = &self.buffer[consumed..];
            if bytes.len() < 2 {
                break;
            }
            let fin = bytes[0] & 0x80 != 0;
            let opcode = bytes[0] & 0xf;
            let compressed = bytes[0] & 0x40 != 0;
            if bytes[0] & 0x30 != 0 || (compressed && (opcode != 1 || self.deflate.is_none())) {
                return Some("websocket_extension_encoding");
            }
            let masked = bytes[1] & 0x80 != 0;
            if masked != client {
                return Some("websocket_invalid_masking");
            }
            let (length, mut header) = match bytes[1] & 0x7f {
                126 => {
                    if bytes.len() < 4 {
                        break;
                    }
                    (u16::from_be_bytes([bytes[2], bytes[3]]) as u64, 4)
                }
                127 => {
                    if bytes.len() < 10 {
                        break;
                    }
                    (
                        u64::from_be_bytes(bytes[2..10].try_into().expect("eight bytes")),
                        10,
                    )
                }
                n => (n as u64, 2),
            };
            if length > MAX_MESSAGE as u64 {
                return Some("websocket_message_limit");
            }
            let mask = if masked {
                if bytes.len() < header + 4 {
                    break;
                }
                let mask: [u8; 4] = bytes[header..header + 4].try_into().expect("four bytes");
                header += 4;
                Some(mask)
            } else {
                None
            };
            if bytes.len() < header + length as usize {
                break;
            }
            if opcode >= 8 {
                if !fin || length > 125 || !matches!(opcode, 8..=10) {
                    return Some("websocket_invalid_control_frame");
                }
            } else {
                let payload = &bytes[header..header + length as usize];
                let decoded: Vec<u8> = payload
                    .iter()
                    .enumerate()
                    .map(|(i, b)| b ^ mask.map_or(0, |m| m[i % 4]))
                    .collect();
                match opcode {
                    1 if self.fragmented.is_none() => {
                        if fin {
                            if let Some(reason) = self.emit(&decoded, compressed, &mut emit) {
                                return Some(reason);
                            }
                        } else {
                            self.fragmented = Some((decoded, compressed));
                        }
                    }
                    0 => {
                        let Some((message, _)) = self.fragmented.as_mut() else {
                            return Some("websocket_unmatched_continuation");
                        };
                        if message.len().saturating_add(decoded.len()) > MAX_MESSAGE {
                            return Some("websocket_message_limit");
                        }
                        message.extend_from_slice(&decoded);
                        if fin {
                            let (message, compressed) =
                                self.fragmented.take().expect("fragmented message");
                            if let Some(reason) = self.emit(&message, compressed, &mut emit) {
                                return Some(reason);
                            }
                        }
                    }
                    _ => return Some("websocket_unsupported_message"),
                }
            }
            consumed += header + length as usize;
        }
        self.buffer.drain(..consumed);
        None
    }
}
