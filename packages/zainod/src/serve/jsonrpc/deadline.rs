//! A deadline on every JSON-RPC call: none waits without end.

use std::time::Duration;

use jsonrpsee::{
    server::middleware::rpc::{layer::ResponseFuture, RpcServiceT},
    types::ErrorObject,
    MethodResponse,
};

/// The code a call that ran out of time answers with: an internal error.
const TIMED_OUT: i32 = -32603;

/// Answers a call still running at `limit` with an error.
#[derive(Clone)]
pub(crate) struct Deadline<S> {
    service: S,
    limit: Duration,
}

impl<S> Deadline<S> {
    pub(crate) fn new(service: S, limit: Duration) -> Self {
        Self { service, limit }
    }
}

impl<'a, S> RpcServiceT<'a> for Deadline<S>
where
    S: RpcServiceT<'a> + Send + Sync + Clone + 'static,
{
    type Future = ResponseFuture<futures::future::BoxFuture<'a, MethodResponse>>;

    fn call(&self, request: jsonrpsee::types::Request<'a>) -> Self::Future {
        let (service, limit) = (self.service.clone(), self.limit);
        let id = request.id.clone().into_owned();
        ResponseFuture::future(Box::pin(async move {
            match tokio::time::timeout(limit, service.call(request)).await {
                Ok(response) => response,
                Err(_) => MethodResponse::error(
                    id,
                    ErrorObject::owned(
                        TIMED_OUT,
                        format!("the request timed out after {}s", limit.as_secs()),
                        None::<()>,
                    ),
                ),
            }
        }))
    }
}
