//! Robin's library. The window is a thin shell over these modules.
//!
//! Mutations go through [`safety::DirFd`] so a path is never opened by string
//! after the parent directory has been checked.

pub mod cli;
pub mod error;
pub mod fixture;
pub mod fsops;
pub mod model;
pub mod safety;
pub mod shots;
pub mod theme;
pub mod thumbs;

pub use error::{Error, Result};

slint::include_modules!();
