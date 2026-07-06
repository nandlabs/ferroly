//! Client connection options.

/// Options for [`WsClient::dial`](ferroly::ws::WsClient::dial).
///
/// Both limits default to `None` (unbounded). When set, an inbound frame or
/// reassembled message exceeding the cap closes the connection. Fields are
/// public — construct with a struct literal or `..Default::default()`.
#[derive(Debug, Clone, Default)]
pub struct WsOptions {
    /// Maximum accepted reassembled message size in bytes (`None` = unbounded).
    pub max_message_size: Option<usize>,
    /// Maximum accepted single-frame payload size in bytes (`None` = unbounded).
    pub max_frame_size: Option<usize>,
}
