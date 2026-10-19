//! Pages and buttons ("the profile"): what each pad shows and does.

pub mod audio;
pub mod images;
pub mod legacy;
pub mod model;
pub mod review;
pub mod service;

pub mod store;

pub use audio::*;
pub use model::*;
pub use service::{mutate, SharedProfile};
pub use store::{LoadFailure, ProfileStore, SaveKind};
