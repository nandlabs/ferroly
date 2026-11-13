//! A connection hub: broadcast to all connections, to a room, or to one
//! connection id.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use super::conn::WsSender;
use super::{Message, WsError};

/// What a [`Hub`] does with a connection whose outgoing queue is full when a
/// broadcast reaches it.
///
/// A broadcast never waits for a slow connection (that would delay every
/// other recipient), so the queue capacity
/// ([`send_queue_capacity`](super::WsServerConfig::send_queue_capacity)) is
/// the whole allowance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SlowConsumerPolicy {
    /// Close the connection with code `1008` and remove it from the hub —
    /// the same outcome as a [`send`](WsSender::send) that times out. The
    /// default: a client that cannot keep up is disconnected rather than
    /// silently missing messages.
    #[default]
    Close,
    /// Skip this message for that connection and keep it connected. Use for
    /// feeds where the latest value supersedes older ones.
    Drop,
}

#[derive(Default)]
struct State {
    members: HashMap<u64, WsSender>,
    rooms: HashMap<String, HashSet<u64>>,
}

struct Inner {
    state: Mutex<State>,
    policy: SlowConsumerPolicy,
}

/// A registry of connections with rooms, for fan-out.
///
/// Register a connection's [`WsSender`] (from
/// [`WsConn::sender`](super::WsConn::sender) or
/// [`WsClient::sender`](super::WsClient::sender)), optionally put it in
/// rooms, and broadcast. The hub is cheap to clone and every clone shares
/// the same state.
///
/// Broadcasts are non-blocking and apply the hub's [`SlowConsumerPolicy`] to
/// connections whose queue is full. Connections that have closed are pruned
/// automatically during broadcasts; [`member`](Self::member) returns a guard
/// that unregisters as soon as the handler returns.
///
/// ```no_run
/// use ferroly::ws::{server, Hub, Message, WsConn, WsServerConfig};
/// use tokio::net::TcpListener;
///
/// # async fn ex() -> Result<(), ferroly::ws::WsError> {
/// let hub = Hub::new();
/// let listener = TcpListener::bind("127.0.0.1:9001").await.unwrap();
/// let rooms = hub.clone();
/// server::serve_with_config(
///     listener,
///     move |mut conn: WsConn| {
///         let hub = rooms.clone();
///         async move {
///             let _member = hub.member(conn.sender()); // leaves on return
///             hub.join(conn.id(), "lobby");
///             while let Some(Message::Text(text)) = conn.recv().await {
///                 hub.broadcast_room("lobby", &Message::text(text));
///             }
///         }
///     },
///     std::future::pending::<()>(),
///     WsServerConfig::default(),
/// )
/// .await
/// # }
/// ```
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state();
        f.debug_struct("Hub")
            .field("members", &state.members.len())
            .field("rooms", &state.rooms.len())
            .field("policy", &self.inner.policy)
            .finish()
    }
}

impl Hub {
    /// A hub with the default [`SlowConsumerPolicy::Close`].
    pub fn new() -> Self {
        Self::with_policy(SlowConsumerPolicy::default())
    }

    /// A hub with an explicit slow-consumer policy.
    pub fn with_policy(policy: SlowConsumerPolicy) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                policy,
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers a connection under its id ([`WsSender::id`]). Registering
    /// the same connection again is a no-op.
    pub fn register(&self, sender: WsSender) {
        self.state().members.insert(sender.id(), sender);
    }

    /// Registers a connection and returns a guard that unregisters it (from
    /// the hub and every room) when dropped.
    pub fn member(&self, sender: WsSender) -> HubMember {
        let id = sender.id();
        self.register(sender);
        HubMember {
            hub: self.clone(),
            id,
        }
    }

    /// Removes a connection from the hub and from every room. Returns
    /// whether it was registered. The connection itself is not closed.
    pub fn unregister(&self, id: u64) -> bool {
        let mut state = self.state();
        let removed = state.members.remove(&id).is_some();
        state.rooms.retain(|_, members| {
            members.remove(&id);
            !members.is_empty()
        });
        removed
    }

    /// Adds a registered connection to `room`, creating the room if needed.
    /// Returns `false` if the connection is not registered.
    pub fn join(&self, id: u64, room: &str) -> bool {
        let mut state = self.state();
        if !state.members.contains_key(&id) {
            return false;
        }
        state.rooms.entry(room.to_string()).or_default().insert(id);
        true
    }

    /// Removes a connection from `room`. Returns whether it was in it. An
    /// empty room is deleted.
    pub fn leave(&self, id: u64, room: &str) -> bool {
        let mut state = self.state();
        let Some(members) = state.rooms.get_mut(room) else {
            return false;
        };
        let removed = members.remove(&id);
        if members.is_empty() {
            state.rooms.remove(room);
        }
        removed
    }

    /// The number of registered connections.
    pub fn len(&self) -> usize {
        self.state().members.len()
    }

    /// Whether no connection is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the connection `id` is registered.
    pub fn contains(&self, id: u64) -> bool {
        self.state().members.contains_key(&id)
    }

    /// The number of connections in `room` (`0` if it does not exist).
    pub fn room_len(&self, room: &str) -> usize {
        self.state().rooms.get(room).map_or(0, HashSet::len)
    }

    /// The names of all non-empty rooms.
    pub fn rooms(&self) -> Vec<String> {
        self.state().rooms.keys().cloned().collect()
    }

    /// Sends `msg` to every registered connection. Returns how many
    /// connections it was queued for.
    pub fn broadcast(&self, msg: &Message) -> usize {
        let targets: Vec<WsSender> = self.state().members.values().cloned().collect();
        self.fan_out(targets, msg)
    }

    /// Sends `msg` to every connection in `room`. Returns how many
    /// connections it was queued for.
    pub fn broadcast_room(&self, room: &str, msg: &Message) -> usize {
        let targets: Vec<WsSender> = {
            let state = self.state();
            match state.rooms.get(room) {
                Some(ids) => ids
                    .iter()
                    .filter_map(|id| state.members.get(id).cloned())
                    .collect(),
                None => Vec::new(),
            }
        };
        self.fan_out(targets, msg)
    }

    /// Sends `msg` to the connection `id`, applying the slow-consumer
    /// policy. Fails with [`WsError::Closed`] if the id is not registered
    /// (or its connection has closed) and with [`WsError::QueueFull`] if
    /// the message could not be queued.
    pub fn send_to(&self, id: u64, msg: Message) -> Result<(), WsError> {
        let target = self.state().members.get(&id).cloned();
        let Some(target) = target else {
            return Err(WsError::Closed);
        };
        self.deliver(&target, msg)
    }

    /// Starts the close handshake on every registered connection and empties
    /// the hub.
    pub fn close_all(&self, code: u16, reason: &str) -> Result<(), WsError> {
        let targets: Vec<WsSender> = {
            let mut state = self.state();
            state.rooms.clear();
            state.members.drain().map(|(_, sender)| sender).collect()
        };
        for target in targets {
            target.close(code, reason)?;
        }
        Ok(())
    }

    /// Queues `msg` on each target. The lock is not held here: sending may
    /// call an observer.
    fn fan_out(&self, targets: Vec<WsSender>, msg: &Message) -> usize {
        targets
            .iter()
            .filter(|target| self.deliver(target, msg.clone()).is_ok())
            .count()
    }

    fn deliver(&self, target: &WsSender, msg: Message) -> Result<(), WsError> {
        match target.try_send(msg) {
            Ok(()) => Ok(()),
            Err(WsError::QueueFull) => {
                if self.inner.policy == SlowConsumerPolicy::Close {
                    target.close_slow_consumer();
                    self.unregister(target.id());
                }
                Err(WsError::QueueFull)
            }
            Err(WsError::Closed) => {
                // Closed (or closing): it will never accept another message.
                self.unregister(target.id());
                Err(WsError::Closed)
            }
            Err(error) => Err(error),
        }
    }
}

/// Keeps a connection registered in a [`Hub`]; dropping it unregisters the
/// connection from the hub and all its rooms. Returned by [`Hub::member`].
#[derive(Debug)]
pub struct HubMember {
    hub: Hub,
    id: u64,
}

impl HubMember {
    /// The connection id this guard covers.
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for HubMember {
    fn drop(&mut self) {
        self.hub.unregister(self.id);
    }
}
