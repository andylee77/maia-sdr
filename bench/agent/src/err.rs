//! Agent error type. Every failure maps to a `code` string the host
//! understands (`bench/fbench/agent.py`: REFUSED_CODES / UNSUPPORTED_CODES)
//! and to a process exit code.

use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// Bad command line (unknown option, missing value).
    Usage,
    /// Unknown subcommand.
    UnknownCommand,
    /// Generic runtime failure.
    Error,
    /// A precondition is not met (service running, wrong state, ...).
    Precondition,
    /// Not supported on this platform/image.
    Unsupported,
    /// Required device (UIO, IIO, rxbuffer) is missing.
    NoDevice,
    /// The bitstream on the board is not the one this operation needs.
    WrongImage,
    /// A named file/register/region does not exist.
    NotFound,
    /// Refused by a safety rule (design doc section 3).
    Safety,
    /// SIGINT/SIGTERM/SIGHUP received.
    Interrupted,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Usage => "usage",
            Code::UnknownCommand => "unknown_command",
            Code::Error => "error",
            Code::Precondition => "precondition",
            Code::Unsupported => "unsupported",
            Code::NoDevice => "no_device",
            Code::WrongImage => "wrong_image",
            Code::NotFound => "not_found",
            Code::Safety => "safety",
            Code::Interrupted => "interrupted",
        }
    }

    /// Process exit code (matches the host's exit-code convention:
    /// 2 error, 3 precondition/unsupported, 4 safety refusal).
    pub fn exit_code(self) -> i32 {
        match self {
            Code::Safety => 4,
            Code::Precondition
            | Code::Unsupported
            | Code::NoDevice
            | Code::WrongImage
            | Code::NotFound
            | Code::UnknownCommand => 3,
            Code::Usage | Code::Error | Code::Interrupted => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentError {
    pub code: Code,
    pub msg: String,
    pub detail: Option<Value>,
}

impl AgentError {
    pub fn new(code: Code, msg: impl Into<String>) -> Self {
        AgentError {
            code,
            msg: msg.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for AgentError {}

pub type AResult<T> = Result<T, AgentError>;

impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        let code = match e.kind() {
            std::io::ErrorKind::NotFound => Code::NotFound,
            std::io::ErrorKind::PermissionDenied => Code::Precondition,
            std::io::ErrorKind::Unsupported => Code::Unsupported,
            _ => Code::Error,
        };
        AgentError::new(code, e.to_string())
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(e: serde_json::Error) -> Self {
        AgentError::new(Code::Error, format!("json: {e}"))
    }
}

impl From<String> for AgentError {
    fn from(s: String) -> Self {
        AgentError::new(Code::Error, s)
    }
}

impl From<&str> for AgentError {
    fn from(s: &str) -> Self {
        AgentError::new(Code::Error, s)
    }
}

/// Adds a message prefix to any displayable error, keeping the code of an
/// `AgentError` / `io::Error`.
pub trait Context<T> {
    fn ctx(self, what: impl fmt::Display) -> AResult<T>;
}

impl<T, E: Into<AgentError>> Context<T> for Result<T, E> {
    fn ctx(self, what: impl fmt::Display) -> AResult<T> {
        self.map_err(|e| {
            let e: AgentError = e.into();
            AgentError {
                code: e.code,
                msg: format!("{what}: {}", e.msg),
                detail: e.detail,
            }
        })
    }
}

#[macro_export]
macro_rules! bail {
    ($code:ident, $($arg:tt)*) => {
        return Err($crate::err::AgentError::new($crate::err::Code::$code, format!($($arg)*)))
    };
}

#[macro_export]
macro_rules! aerr {
    ($code:ident, $($arg:tt)*) => {
        $crate::err::AgentError::new($crate::err::Code::$code, format!($($arg)*))
    };
}
