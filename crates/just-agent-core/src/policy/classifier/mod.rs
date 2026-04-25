//! AST-based shell command safety classifier using rable.

mod delegate;
mod helpers;
mod lists;
mod util;
mod walker;

#[cfg(test)]
mod tests;

pub use walker::classify_command;
