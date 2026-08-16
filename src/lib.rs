//! Cordis in Rust: spatiotemporal composability.
//!
//! Revertible effects (every context transformation carries an inverse the
//! runtime tracks and applies in reverse on unmount) + reactive coeffects
//! (a component declares the context keys it reads; on each change the runtime
//! notifies exactly the matching components). Lua is the scripting source over
//! the Rust kernel.

pub mod core;
pub mod lua;

pub use core::{
    Context, Fiber, FiberState, Shared,
};
pub use lua::{Ctx, FiberHandle, LuaContext, EffectHandle};
