use std::path::PathBuf;

/// Errors produced by chip-core operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a chip repository (or any parent): {0}")]
    NotARepo(PathBuf),

    #[error("a chip repository already exists at {0}")]
    RepoExists(PathBuf),

    #[error("object {0} not found")]
    ObjectNotFound(String),

    #[error("invalid object id: {0}")]
    InvalidObjectId(String),

    #[error("reference not found: {0}")]
    RefNotFound(String),

    #[error("no commits yet")]
    EmptyHistory,

    #[error("unexpected object kind: expected {expected}, found {found}")]
    WrongObjectKind {
        expected: &'static str,
        found: &'static str,
    },

    #[error("merge has no common ancestor")]
    NoMergeBase,

    #[error("serialization error: {0}")]
    Serialize(#[from] Box<bincode::ErrorKind>),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
