//! Core of Social Media Connector: networks, storage, credentials and the
//! publishing scheduler. The Tauri app in the parent crate is a thin layer
//! over [`service::Service`].

pub mod error;
pub mod http;
pub mod media;
pub mod model;
pub mod oauth;
pub mod providers;
pub mod secrets;
pub mod service;
pub mod store;
pub mod text;

#[cfg(test)]
pub mod testutil;
