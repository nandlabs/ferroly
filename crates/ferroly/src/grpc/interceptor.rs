//! [`Interceptor`]: code that runs before every call, on a
//! [`Channel`](super::Channel) or a [`GrpcServer`](super::GrpcServer).

use super::call::Request;
use super::status::Status;

/// Runs before every call — on a [`Channel`](super::Channel) before the
/// request is sent, on a [`GrpcServer`](super::GrpcServer) before the
/// handler runs — with the method path and the call's head: its
/// [`Metadata`](super::Metadata), timeout and deadline (and, on a server,
/// the peer). It may change the metadata (add an auth token, a trace id),
/// set or tighten the timeout on a client, or reject the call with a
/// [`Status`], which then fails the call without sending it (client) or
/// without running the handler (server).
///
/// Interceptors run in the order they were added. Any
/// `Fn(&str, &mut Request<()>) -> Result<(), Status>` is one:
///
/// ```
/// use ferroly::grpc::{Channel, GrpcServer, Request, Status};
///
/// // Client: attach a token to every call.
/// let channel = Channel::builder("http://127.0.0.1:50051")?
///     .interceptor(|_path: &str, req: &mut Request<()>| {
///         req.metadata_mut().insert("authorization", "Bearer t0ken")?;
///         Ok(())
///     })
///     .build()?;
///
/// // Server: reject calls without one.
/// let server = GrpcServer::new().interceptor(|_path: &str, req: &mut Request<()>| {
///     match req.metadata().get("authorization") {
///         Some(v) if v == "Bearer t0ken" => Ok(()),
///         _ => Err(Status::unauthenticated("missing or bad token")),
///     }
/// });
/// # let _ = (channel, server);
/// # Ok::<(), Status>(())
/// ```
pub trait Interceptor: Send + Sync + 'static {
    /// Inspects or changes the head of the call to `method`
    /// (`/package.Service/Method`); an `Err` fails the call with that status.
    fn intercept(&self, method: &str, request: &mut Request<()>) -> Result<(), Status>;
}

impl<F> Interceptor for F
where
    F: Fn(&str, &mut Request<()>) -> Result<(), Status> + Send + Sync + 'static,
{
    fn intercept(&self, method: &str, request: &mut Request<()>) -> Result<(), Status> {
        self(method, request)
    }
}

/// Runs `interceptors` in order on `head`.
pub(crate) fn run(
    interceptors: &[std::sync::Arc<dyn Interceptor>],
    method: &str,
    head: &mut Request<()>,
) -> Result<(), Status> {
    for interceptor in interceptors {
        interceptor.intercept(method, head)?;
    }
    Ok(())
}
