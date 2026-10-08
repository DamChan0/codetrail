//! ct-core: read-only git access (via the `git` CLI), diff parsing, search and ref syntax.

mod diffparse;
pub(crate) mod git;
mod repo;
mod search;
mod types;

pub use git::{git_slots, GitSlots, MAX_GIT_PROCS};
pub use search::{search_content, FileIndex};
pub use types::*;

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("not a git repository: {0}")]
    NotARepo(String),
    #[error("`{cmd}` failed (exit {code}): {stderr}")]
    Git { cmd: String, code: i32, stderr: String },
    #[error("failed to run git: {0}")]
    Spawn(String),
    #[error("git command timed out after {0:?}")]
    Timeout(Duration),
    #[error("`{cmd}` produced more than {limit} bytes of output")]
    TooLarge { cmd: String, limit: usize },
    #[error("cancelled")]
    Cancelled,
    #[error("search error: {0}")]
    Search(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
