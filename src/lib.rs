//! Robin's library. The window is a thin shell over these modules.
//!
//! Mutations go through [`safety::DirFd`] so a path is never opened by string
//! after the parent directory has been checked.

pub mod app;
pub mod cli;
pub mod delegate;
pub mod error;
pub mod fixture;
pub mod fsops;
pub mod instance;
pub mod model;
pub mod mounts;
pub mod ops;
pub mod prefs;
pub mod render;
pub mod safety;
pub mod shots;
pub mod sidebar;
pub mod stats;
pub mod theme;
pub mod thumbs;
pub mod trash;
pub mod video;
pub mod watch;

pub use error::{Error, Result};

slint::include_modules!();
