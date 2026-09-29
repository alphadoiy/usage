//! Grok 账单响应没有公开 proto，按字段路径启发式提取已用百分比和重置时间。

#[derive(Default)]
struct ProtobufScan {
    fixed32_fields: Vec<(Vec<u64>, f32, usize)>,
    varint_fields: Vec<(Vec<u64>, u64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BillingSnapshot {
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}

pub fn parse_billing_payload(data: &[u8], now_secs: i64) -> Result<BillingSnapshot, String> {
    let mut payloads = grpc_web_data_frames(data);
    if payloads.is_empty() && looks_like_protobuf(data) {
        payloads = vec![data];
    }
    if payloads.is_empty() {
        return Err("账单响应里没有 protobuf 数据".into());
    }
    let mut scan = ProtobufScan::default();
    for payload in payloads {
        scan_protobuf(payload, 0, &[], 0, &mut scan);
    }

    let mut parsed_percent = None;
    let mut best: Option<(usize, usize)> = None;
    for (path, value, order) in &scan.fixed32_fields {
        if path.last() != Some(&1) || !in_percent_range(*value) {
            continue;
        }
        let rank = (path.len(), *order);
        if best.is_none_or(|current| rank < current) {
            best = Some(rank);
            parsed_percent = Some(f64::from(*value));
        }
    }

    let reset_candidates: Vec<(&[u64], i64)> = scan
        .varint_fields
        .iter()
        .filter(|(_, value)| {
            (1_700_000_000..=2_100_000_000).contains(value) && *value as i64 > now_secs
        })
        .map(|(path, value)| (path.as_slice(), *value as i64))
        .collect();
    let reset = reset_candidates
        .iter()
        .find(|(path, _)| *path == [1, 5, 1])
        .map(|(_, ts)| *ts)
        .or_else(|| reset_candidates.iter().map(|(_, ts)| *ts).min());

    let has_usage_period = scan.varint_fields.iter().any(|(path, value)| {
        path.starts_with(&[1, 6]) || (path.as_slice() == [1, 8, 1] && (*value == 1 || *value == 2))
    });
    let no_usage_yet = parsed_percent.is_none()
        && scan.fixed32_fields.is_empty()
        && reset.is_some()
        && has_usage_period;
    let used_percent = parsed_percent
        .or(if no_usage_yet { Some(0.0) } else { None })
        .ok_or_else(|| "账单响应里没有用量百分比".to_string())?;
    Ok(BillingSnapshot {
        used_percent,
        resets_at: reset,
    })
}

pub fn grpc_web_data_frames(data: &[u8]) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    let mut index = 0;
    while index < data.len() {
        if index + 5 > data.len() {
            return Vec::new();
        }
        let flags = data[index];
        let length =
            u32::from_be_bytes(data[index + 1..index + 5].try_into().unwrap_or([0; 4])) as usize;
        let start = index + 5;
        let end = start + length;
        if end > data.len() {
            return Vec::new();
        }
        if flags & 0x80 == 0 {
            frames.push(&data[start..end]);
        }
        index = end;
    }
    frames
}

pub fn grpc_web_trailer_fields(data: &[u8]) -> std::collections::HashMap<String, String> {
    let mut fields = std::collections::HashMap::new();
    let mut index = 0;
    while index + 5 <= data.len() {
        let flags = data[index];
        let length =
            u32::from_be_bytes(data[index + 1..index + 5].try_into().unwrap_or([0; 4])) as usize;
        let start = index + 5;
        let end = start + length;
        if end > data.len() {
            break;
        }
        if flags & 0x80 != 0 {
            let text = String::from_utf8_lossy(&data[start..end]);
            for line in text.lines() {
                if let Some((key, value)) = line.split_once(':')
                    && !line.is_empty()
                {
                    fields.insert(
                        key.trim().to_ascii_lowercase(),
                        percent_decode(value.trim()),
                    );
                }
            }
        }
        index = end;
    }
    fields
}

pub fn percent_decode(value: &str) -> String {
    let raw = value.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' && index + 2 < raw.len() {
            let piece = &raw[index + 1..index + 3];
            if let Ok(hex) = std::str::from_utf8(piece)
                && let Ok(byte) = u8::from_str_radix(hex, 16)
            {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(raw[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn looks_like_protobuf(data: &[u8]) -> bool {
    let Some(&first) = data.first() else {
        return false;
    };
    let field_number = first >> 3;
    let wire_type = first & 0x07;
    field_number > 0 && matches!(wire_type, 0 | 1 | 2 | 5)
}

fn scan_protobuf(
    data: &[u8],
    depth: usize,
    path: &[u64],
    order: usize,
    scan: &mut ProtobufScan,
) -> usize {
    let mut index = 0;
    let mut next_order = order;
    while index < data.len() {
        let field_start = index;
        let Some((key, next_index)) = read_varint(data, index) else {
            index = field_start + 1;
            continue;
        };
        if key == 0 {
            index = field_start + 1;
            continue;
        }
        index = next_index;
        let field_number = key >> 3;
        let wire_type = key & 0x07;
        let mut field_path = path.to_vec();
        field_path.push(field_number);
        match wire_type {
            0 => match read_varint(data, index) {
                Some((value, next_index)) => {
                    index = next_index;
                    scan.varint_fields.push((field_path, value));
                }
                None => index = field_start + 1,
            },
            1 => {
                if index + 8 > data.len() {
                    return next_order;
                }
                index += 8;
            }
            2 => match read_varint(data, index) {
                Some((length, next_index)) if length as usize <= data.len() - next_index => {
                    index = next_index;
                    let end = index + length as usize;
                    if depth < 4 {
                        next_order = scan_protobuf(
                            &data[index..end],
                            depth + 1,
                            &field_path,
                            next_order,
                            scan,
                        );
                    }
                    index = end;
                }
                _ => index = field_start + 1,
            },
            5 => {
                if index + 4 > data.len() {
                    return next_order;
                }
                let bits = u32::from_le_bytes(data[index..index + 4].try_into().unwrap_or([0; 4]));
                scan.fixed32_fields
                    .push((field_path, f32::from_bits(bits), next_order));
                next_order += 1;
                index += 4;
            }
            _ => index = field_start + 1,
        }
    }
    next_order
}

pub fn tier_name_for_reset(resets_at: Option<i64>, now_secs: i64) -> &'static str {
    if let Some(ts) = resets_at {
        let days = ((ts - now_secs) as f64 / 86400.0).round() as i64;
        if (4..=12).contains(&days) {
            return "weekly_limit";
        }
        if (20..=45).contains(&days) {
            return "monthly";
        }
    }
    "credits"
}

pub fn is_grpc_auth_failure(status: i64, message: &str) -> bool {
    if status == 16 {
        return true;
    }
    if status != 7 {
        return false;
    }
    let lower = message.to_ascii_lowercase();
    lower.contains("bad-credentials")
        || lower.contains("unauthenticated")
        || (lower.contains("oauth2") && lower.contains("could not be validated"))
        || (lower.contains("access token")
            && (lower.contains("invalid")
                || lower.contains("expired")
                || lower.contains("could not be validated")))
}

pub fn is_team_billing_unavailable(status: i64, message: &str) -> bool {
    status == 9
        && matches!(
            message.trim().to_ascii_lowercase().as_str(),
            "no personal team" | "no personal team."
        )
}

pub fn is_transient_grpc_status(status: i64, message: &str) -> bool {
    match status {
        4 | 14 => true,
        1 => {
            let lower = message.to_ascii_lowercase();
            lower.contains("timeout") || lower.contains("deadline") || lower.contains("expired")
        }
        _ => false,
    }
}

fn read_varint(data: &[u8], mut index: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    while index < data.len() && shift < 64 {
        let byte = data[index];
        index += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some((value, index));
        }
        shift += 7;
    }
    None
}

fn in_percent_range(value: f32) -> bool {
    value.is_finite() && (0.0..=100.0).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_750_000_000;

    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out
    }

    fn field_varint(number: u64, value: u64) -> Vec<u8> {
        let mut out = varint(number << 3);
        out.extend(varint(value));
        out
    }

    fn field_float(number: u64, value: f32) -> Vec<u8> {
        let mut out = varint((number << 3) | 5);
        out.extend(value.to_bits().to_le_bytes());
        out
    }

    fn field_message(number: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = varint((number << 3) | 2);
        out.extend(varint(payload.len() as u64));
        out.extend(payload);
        out
    }

    fn frame(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![flags];
        out.extend((payload.len() as u32).to_be_bytes());
        out.extend(payload);
        out
    }

    #[test]
    fn framed_percent_and_reset() {
        let reset_ts = (NOW + 30 * 86_400) as u64;
        let inner = [
            field_float(1, 37.5),
            field_message(5, &field_varint(1, reset_ts)),
        ]
        .concat();
        let snapshot = parse_billing_payload(&frame(0, &field_message(1, &inner)), NOW).unwrap();
        assert_eq!(snapshot.used_percent, 37.5);
        assert_eq!(snapshot.resets_at, Some(reset_ts as i64));
    }

    #[test]
    fn prefers_shallow_percent() {
        let inner = [
            field_message(2, &field_float(1, 99.0)),
            field_float(1, 25.0),
        ]
        .concat();
        let snapshot = parse_billing_payload(&frame(0, &field_message(1, &inner)), NOW).unwrap();
        assert_eq!(snapshot.used_percent, 25.0);
    }

    #[test]
    fn zero_usage_without_percent_field() {
        let reset_ts = (NOW + 7 * 86_400) as u64;
        let inner = [
            field_message(5, &field_varint(1, reset_ts)),
            field_message(6, &field_varint(1, 3)),
        ]
        .concat();
        let snapshot = parse_billing_payload(&frame(0, &field_message(1, &inner)), NOW).unwrap();
        assert_eq!(snapshot.used_percent, 0.0);
        assert_eq!(snapshot.resets_at, Some(reset_ts as i64));
    }

    #[test]
    fn percent_decode_and_tier_names() {
        assert_eq!(percent_decode("no%20personal%20team"), "no personal team");
        assert_eq!(percent_decode("bad%1é"), "bad%1é");
        assert_eq!(
            tier_name_for_reset(Some(NOW + 7 * 86_400), NOW),
            "weekly_limit"
        );
        assert_eq!(tier_name_for_reset(Some(NOW + 30 * 86_400), NOW), "monthly");
        assert_eq!(tier_name_for_reset(None, NOW), "credits");
        assert!(is_grpc_auth_failure(16, ""));
        assert!(is_transient_grpc_status(4, ""));
        assert!(is_team_billing_unavailable(9, " No Personal Team "));
    }
}
