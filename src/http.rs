use std::collections::HashMap;

pub const MAX_HEADER_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    /// Number of bytes belonging to this request in the input buffer.
    pub consumed: usize,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn path_and_query(&self) -> (&str, &str) {
        self.target.split_once('?').unwrap_or((&self.target, ""))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpError {
    BadRequest,
    PayloadTooLarge,
}

pub fn parse_request(input: &[u8], max_body: usize) -> Result<Option<Request>, HttpError> {
    let Some(header_end) = find_bytes(input, b"\r\n\r\n") else {
        return if input.len() > MAX_HEADER_BYTES {
            Err(HttpError::BadRequest)
        } else {
            Ok(None)
        };
    };
    if header_end > MAX_HEADER_BYTES {
        return Err(HttpError::BadRequest);
    }
    let raw_headers =
        std::str::from_utf8(&input[..header_end]).map_err(|_| HttpError::BadRequest)?;
    let mut lines = raw_headers.split("\r\n");
    let request_line = lines.next().ok_or(HttpError::BadRequest)?;
    let mut request_parts = request_line.split_ascii_whitespace();
    let method = request_parts.next().ok_or(HttpError::BadRequest)?;
    let target = request_parts.next().ok_or(HttpError::BadRequest)?;
    let version = request_parts.next().ok_or(HttpError::BadRequest)?;
    if request_parts.next().is_some()
        || method.is_empty()
        || !method.bytes().all(is_token_byte)
        || !target.starts_with('/')
        || target.contains('#')
        || !matches!(version, "HTTP/1.1" | "HTTP/1.0")
    {
        return Err(HttpError::BadRequest);
    }

    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(HttpError::BadRequest)?;
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(HttpError::BadRequest);
        }
        let key = name.to_ascii_lowercase();
        let value = value.trim_matches([' ', '\t']);
        if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(HttpError::BadRequest);
        }
        if let Some(previous) = headers.get(&key) {
            if key == "content-length" && previous != value {
                return Err(HttpError::BadRequest);
            }
            let combined = format!("{previous}, {value}");
            headers.insert(key, combined);
        } else {
            headers.insert(key, value.to_string());
        }
    }

    let body_start = header_end + 4;
    let transfer_encoding = headers
        .get("transfer-encoding")
        .map(|s| s.to_ascii_lowercase());
    let content_length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>().map_err(|_| HttpError::BadRequest))
        .transpose()?;
    let (body, consumed) = if let Some(encoding) = transfer_encoding {
        if content_length.is_some() || encoding.trim() != "chunked" {
            return Err(HttpError::BadRequest);
        }
        match decode_chunked(&input[body_start..], max_body)? {
            Some((body, body_bytes)) => (body, body_start + body_bytes),
            None => return Ok(None),
        }
    } else if let Some(length) = content_length {
        if length > max_body {
            return Err(HttpError::PayloadTooLarge);
        }
        let available = input.len().saturating_sub(body_start);
        if available < length {
            return Ok(None);
        }
        (
            input[body_start..body_start + length].to_vec(),
            body_start + length,
        )
    } else {
        (Vec::new(), body_start)
    };

    Ok(Some(Request {
        method: method.to_string(),
        target: target.to_string(),
        version: version.to_string(),
        headers,
        body,
        consumed,
    }))
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_chunked(input: &[u8], max_body: usize) -> Result<Option<(Vec<u8>, usize)>, HttpError> {
    let mut cursor = 0;
    let mut body = Vec::new();
    loop {
        let Some(line_end) = find_bytes(&input[cursor..], b"\r\n") else {
            return Ok(None);
        };
        let line = std::str::from_utf8(&input[cursor..cursor + line_end])
            .map_err(|_| HttpError::BadRequest)?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        if size_text.is_empty() || !size_text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(HttpError::BadRequest);
        }
        let size = usize::from_str_radix(size_text, 16).map_err(|_| HttpError::BadRequest)?;
        cursor += line_end + 2;
        if size == 0 {
            loop {
                let Some(trailer_end) = find_bytes(&input[cursor..], b"\r\n") else {
                    return Ok(None);
                };
                if trailer_end == 0 {
                    return Ok(Some((body, cursor + 2)));
                }
                let trailer = &input[cursor..cursor + trailer_end];
                if !trailer.contains(&b':') {
                    return Err(HttpError::BadRequest);
                }
                cursor += trailer_end + 2;
            }
        }
        if body.len().saturating_add(size) > max_body {
            return Err(HttpError::PayloadTooLarge);
        }
        let Some(chunk_end) = cursor.checked_add(size) else {
            return Err(HttpError::PayloadTooLarge);
        };
        if input.len() < chunk_end + 2 {
            return Ok(None);
        }
        if &input[chunk_end..chunk_end + 2] != b"\r\n" {
            return Err(HttpError::BadRequest);
        }
        body.extend_from_slice(&input[cursor..chunk_end]);
        cursor = chunk_end + 2;
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_request, HttpError};

    #[test]
    fn parses_headers_method_and_content_length_body() {
        let raw = b"POST /items?q=1 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\ndata";
        let request = parse_request(raw, 16).unwrap().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path_and_query(), ("/items", "q=1"));
        assert_eq!(request.header("HOST"), Some("localhost"));
        assert_eq!(request.body, b"data");
        assert_eq!(request.consumed, raw.len());
    }

    #[test]
    fn accepts_delete_and_waits_for_partial_input() {
        assert!(parse_request(b"DELETE /x HTTP/1.1\r\nHost: a\r\n", 16)
            .unwrap()
            .is_none());
        let request = parse_request(b"DELETE /x HTTP/1.1\r\nHost: a\r\n\r\n", 16)
            .unwrap()
            .unwrap();
        assert_eq!(request.method, "DELETE");
    }

    #[test]
    fn decodes_chunked_body_and_trailers() {
        let raw = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\ntest\r\n0\r\nX-Check: yes\r\n\r\n";
        let request = parse_request(raw, 8).unwrap().unwrap();
        assert_eq!(request.body, b"test");
        assert_eq!(request.consumed, raw.len());
        assert!(parse_request(
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nte",
            8
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn leaves_pipelined_request_bytes_for_the_next_parse() {
        let first = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let second = b"DELETE /x HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let both = [first.as_slice(), second.as_slice()].concat();
        let parsed_first = parse_request(&both, 16).unwrap().unwrap();
        assert_eq!(parsed_first.consumed, first.len());
        let parsed_second = parse_request(&both[parsed_first.consumed..], 16)
            .unwrap()
            .unwrap();
        assert_eq!(parsed_second.method, "DELETE");
        assert_eq!(parsed_second.consumed, second.len());
    }

    #[test]
    fn rejects_malformed_and_oversized_requests() {
        assert_eq!(
            parse_request(b"GET / HTTP/1.1\r\nBroken\r\n\r\n", 8).unwrap_err(),
            HttpError::BadRequest
        );
        assert_eq!(
            parse_request(b"POST / HTTP/1.1\r\nContent-Length: 9\r\n\r\n", 8).unwrap_err(),
            HttpError::PayloadTooLarge
        );
    }
}
