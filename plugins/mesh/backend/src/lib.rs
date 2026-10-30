//! `cc-mesh`: the mesh@1 plugin. This person's CortexCode instances linked by key: pairing with a
//! short code, a signed membership log, encrypted connections over the local network or a relay,
//! capability cards, a member's window working in this computer's projects, and borrowing a
//! member's model (one tagged for the request's work) through the frame's own `model` contract, and
//! the services its plugins share (`services`).

pub mod borrow;
pub mod card;
pub mod claims;
pub mod identity;
#[cfg(target_os = "macos")]
pub mod keychain;
pub mod local;
pub mod members;
pub mod net;
pub mod pairing;
pub mod place;
pub mod projects;
pub mod service;
pub mod services;
pub mod store;
pub mod words;

/// The app this mesh plugin belongs to, as its card and its local file name it.
pub const SOFTWARE: &str = "cortexcode";

/// The app as the Keychain names it (`<APP> mesh identity`).
pub const APP: &str = "CortexCode";
