pub mod cgi;
pub mod config;
pub mod http;
pub mod server;

pub fn status_reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Internal Server Error",
    }
}

#[cfg(test)]
mod status_tests {
    use super::status_reason;

    #[test]
    fn maps_required_error_statuses() {
        assert_eq!(status_reason(400), "Bad Request");
        assert_eq!(status_reason(403), "Forbidden");
        assert_eq!(status_reason(404), "Not Found");
        assert_eq!(status_reason(405), "Method Not Allowed");
        assert_eq!(status_reason(413), "Payload Too Large");
        assert_eq!(status_reason(500), "Internal Server Error");
    }
}
