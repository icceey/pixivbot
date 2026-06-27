use std::fmt;

#[derive(Debug)]
pub enum Error {
    Http(reqwest::Error),
    Json(serde_json::Error),
    Api { message: String, status: u16 },
    Parse(String),
    Io(std::io::Error),
    Zip(String),
    /// HTTP 429 Too Many Requests. `retry_after_secs` is parsed from Retry-After header.
    RateLimited { retry_after_secs: Option<u64> },
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(e) => write!(f, "HTTP error: {}", e),
            Error::Json(e) => write!(f, "JSON parse error: {}", e),
            Error::Api { message, status } => write!(f, "API error ({}): {}", status, message),
            Error::Parse(msg) => write!(f, "Parse error: {}", msg),
            Error::Io(e) => write!(f, "IO error: {}", e),
            Error::Zip(msg) => write!(f, "ZIP error: {}", msg),
            Error::RateLimited { retry_after_secs } => {
                write!(f, "Rate limited (429), retry after {:?}", retry_after_secs)
            }
            Error::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Http(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Error::Http(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Json(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<zip::result::ZipError> for Error {
    fn from(err: zip::result::ZipError) -> Self {
        Error::Zip(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
