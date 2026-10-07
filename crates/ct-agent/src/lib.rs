//! ct-agent: Claude Code hook handler, agent installers, `note`/`record`/`ask`/`export` CLI.
//!
//! The binary crate (`ct-app`) forwards `install | hook | note | record | ask | export` to [`run_cli`].

pub mod ask;
pub mod cli;
pub mod gitx;
pub mod hook;
pub mod install;

pub use cli::{dispatch, run_cli, SUBCOMMANDS};
