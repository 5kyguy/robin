//! Robin's library. The window is a thin shell over these modules.
//!
//! Mutations go through [`safety::DirFd`] so a path is never opened by string
//! after the parent directory has been checked.

pub mod app;
pub mod cli;
pub mod clipboard;
pub mod delegate;
pub mod devices;
pub mod error;
pub mod fixture;
pub mod fsops;
pub mod gvfs;
pub mod instance;
pub mod menus;
pub mod model;
pub mod mounts;
pub mod mpv;
pub mod ops;
pub mod picture;
pub mod player;
pub mod prefs;
pub mod rename;
pub mod render;
pub mod safety;
pub mod shots;
pub mod sidebar;
pub mod stats;
pub mod theme;
pub mod thumbs;
pub mod toast;
pub mod trash;
pub mod video;
pub mod viewer;
pub mod watch;

pub use error::{Error, Result};

slint::include_modules!();
