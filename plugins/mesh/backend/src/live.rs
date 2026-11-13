//! A member's live connections (`cortex/live/1`, [`crate::net::LiveHandler`]). This app carries
//! no live media: the connection is closed at once, and the member's app stops asking.

use crate::service::Mesh;
use iroh::endpoint::Connection;
use std::sync::Arc;

/// Serve a member's live connection, already admitted as a member's: this app closes it.
pub async fn serve_live(_mesh: &Arc<Mesh>, _node: &str, connection: Connection) {
    connection.close(3u32.into(), b"no live media here");
}
