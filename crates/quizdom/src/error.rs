use std::fmt;
use std::io;

#[derive(Debug)]
pub enum QuizdomError {
    Io(io::Error),
    Aida(String),
    // trace:STORY-205 | ai:claude — dolt CLI failures during db bootstrap.
    Dolt(String),
    Parse(String),
    Usage(String),
}

impl fmt::Display for QuizdomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Aida(message)
            | Self::Dolt(message)
            | Self::Parse(message)
            | Self::Usage(message) => {
                write!(f, "{message}")
            }
        }
    }
}

impl std::error::Error for QuizdomError {}

impl From<io::Error> for QuizdomError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, QuizdomError>;
