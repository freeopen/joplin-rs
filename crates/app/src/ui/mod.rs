//! Self-contained GPUI widgets for the note browser.
//!
//! Design conventions (element structure, state ownership by parent view,
//! dialog overlay layering) reference longbridge/gpui-kit (Apache-2.0) and
//! Zed's official gpui examples; implementations here target gpui 0.2.2
//! directly with no gpui-kit dependency.

pub mod input;
pub mod markdown;
