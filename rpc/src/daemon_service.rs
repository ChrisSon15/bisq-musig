//! The internal service `musigd.internal.Daemon`: requests that concern the `musigd` process
//! itself. It is not part of the official APIs, which are the trade service and the wallet
//! service.

use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Result};
use tracing::{info, instrument};

pub use crate::pb::musigd_internal::daemon_server::DaemonServer;
use crate::pb::musigd_internal::{ShutdownRequest, ShutdownResponse, daemon_server};

/// The service `musigd.internal.Daemon`.
pub struct DaemonImpl {
    /// The request to end the process in the regular way
    /// (`docs/specifications/trade/shutdown.md`). The request `Shutdown` cancels it. Its owner
    /// stops the server then, and completes the running requests first, so the caller of
    /// `Shutdown` gets the response.
    pub shutdown: CancellationToken,
}

#[tonic::async_trait]
impl daemon_server::Daemon for DaemonImpl {
    #[instrument(skip_all)]
    async fn shutdown(
        &self,
        _request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>> {
        info!("Got the request to shut down.");
        self.shutdown.cancel();
        Ok(Response::new(ShutdownResponse {}))
    }
}
