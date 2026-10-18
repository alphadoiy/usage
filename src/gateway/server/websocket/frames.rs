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
        mut bytes: &[u8],
        client: bool,
        mut emit: impl FnMut(&[u8]),
    ) -> Option<&'static str> {
        loop {
            // Buffer only one frame at a time. A TCP read may contain the tail of
            // a maximum-sized frame and any number of following valid frames.
            if !self.fill(&mut bytes, 2) {
                return None;
            }
            let length_header = match self.buffer[1] & 0x7f {
                126 => 4,
                127 => 10,
                _ => 2,
            };
            if !self.fill(&mut bytes, length_header) {
                return None;
            }
            let bytes_header = &self.buffer;
            let length = match length_header {
                4 => u16::from_be_bytes([bytes_header[2], bytes_header[3]]) as u64,
                10 => u64::from_be_bytes(bytes_header[2..10].try_into().expect("eight bytes")),
                _ => (bytes_header[1] & 0x7f) as u64,
            };
            if length > MAX_MESSAGE as u64 {
                return Some("websocket_message_limit");
            }
            let header = length_header + if bytes_header[1] & 0x80 != 0 { 4 } else { 0 };
            if !self.fill(&mut bytes, header + length as usize) {
                return None;
            }
            let frame = &self.buffer;
            let fin = frame[0] & 0x80 != 0;
            let opcode = frame[0] & 0xf;
            let compressed = frame[0] & 0x40 != 0;
            if frame[0] & 0x30 != 0 || (compressed && (opcode != 1 || self.deflate.is_none())) {
                return Some("websocket_extension_encoding");
            }
            let masked = frame[1] & 0x80 != 0;
            if masked != client {
                return Some("websocket_invalid_masking");
            }
            let mask = if masked {
                Some(<[u8; 4]>::try_from(&frame[length_header..header]).expect("four bytes"))
            } else {
                None
            };
            if opcode >= 8 {
                if !fin || length > 125 || !matches!(opcode, 8..=10) {
                    return Some("websocket_invalid_control_frame");
                }
            } else {
                let payload = &frame[header..];
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
            self.buffer.clear();
            if bytes.is_empty() {
                return None;
            }
        }
    }
    fn fill(&mut self, bytes: &mut &[u8], target: usize) -> bool {
        let take = target.saturating_sub(self.buffer.len()).min(bytes.len());
        self.buffer.extend_from_slice(&bytes[..take]);
        *bytes = &bytes[take..];
        self.buffer.len() >= target
    }
}
