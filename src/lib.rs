mod conn;
mod registry;
mod room;
mod signal;

use std::sync::Arc;
use tokio::sync::RwLock;
use y_sync::awareness::Awareness;

pub type AwarenessRef = Arc<RwLock<Awareness>>;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
pub type Topic = Arc<str>;
pub type PeerId = Arc<str>;
