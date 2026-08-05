pub mod admission;
mod affinity;
pub mod app;
mod backend;
pub mod config;
mod error;
// P1-07で旧routing pathと一緒に削除するまでの互換module。
#[allow(dead_code)]
mod heartbeat;
mod metrics;
mod persistence;
pub mod proxy;
mod routing;
pub mod target;
