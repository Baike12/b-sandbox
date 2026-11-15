//! b-sandbox REST service library: routers, state, and the file data plane.
//! The binary in `main.rs` is a thin bootstrap over this library, which lets
//! integration tests drive the exact same router the service runs.

pub mod b64;
pub mod error;
pub mod files;
pub mod routes;
pub mod state;
