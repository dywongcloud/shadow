//! [`ProtocolHandler`] implementation for the docs [`Engine`].

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use iroh::{Endpoint, endpoint::Connection, protocol::ProtocolHandler};
use iroh_blobs::api::Store as BlobsStore;
use iroh_gossip::net::Gossip;

use crate::{
    api::DocsApi,
    engine::{DefaultAuthorStorage, Engine, ProtectCallbackHandler},
    store::Store,
};

#[derive(Default, Debug)]
enum Storage {
    #[default]
    Memory,
    #[cfg(feature = "fs-store")]
    Persistent(std::path::PathBuf),
}

#[derive(Debug, Default)]
struct ShutdownState {
    started: AtomicBool,
    result: Mutex<Option<std::result::Result<(), String>>>,
    complete: tokio::sync::Notify,
}

/// Docs protocol.
#[derive(Debug, Clone)]
pub struct Docs {
    engine: Arc<Engine>,
    api: DocsApi,
    shutdown: Arc<ShutdownState>,
}

impl Docs {
    /// Create a new [`Builder`] for the docs protocol, using in memory replica and author storage.
    pub fn memory() -> Builder {
        Builder::default()
    }

    /// Create a new [`Builder`] for the docs protocol, using a persistent replica and author storage
    /// in the given directory.
    #[cfg(feature = "fs-store")]
    pub fn persistent(path: std::path::PathBuf) -> Builder {
        Builder {
            storage: Storage::Persistent(path),
            protect_cb: None,
            mutation_gate: None,
        }
    }

    /// Creates a new [`Docs`] from an [`Engine`].
    pub fn new(engine: Engine) -> Self {
        let engine = Arc::new(engine);
        let api = DocsApi::spawn(engine.clone());
        Self {
            engine,
            api,
            shutdown: Arc::new(ShutdownState::default()),
        }
    }

    /// Shut the live engine down exactly once and report the final store-flush
    /// outcome to every caller. Router shutdown invokes the same path through
    /// `ProtocolHandler`; an owner may await it afterwards to propagate errors.
    pub async fn shutdown(&self) -> Result<()> {
        let owner = self
            .shutdown
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if owner {
            // The first caller starts an owned supervisor instead of polling the
            // Engine directly. Dropping a Router/backend shutdown future therefore
            // cannot strand `started=true` with no result; the actor keeps flushing,
            // and a later caller resumes at the same completion notification.
            let engine = self.engine.clone();
            let shutdown = self.shutdown.clone();
            tokio::spawn(async move {
                let engine_task = tokio::spawn(async move { engine.shutdown().await });
                let result = match engine_task.await {
                    Ok(result) => result.map_err(|error| error.to_string()),
                    Err(join_error) => Err(format!(
                        "iroh-docs Engine shutdown task terminated with failure: {join_error}"
                    )),
                };
                *shutdown
                    .result
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
                shutdown.complete.notify_waiters();
            });
        }

        loop {
            let notified = self.shutdown.complete.notified();
            if let Some(result) = self
                .shutdown
                .result
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            {
                return result.map_err(anyhow::Error::msg);
            }
            notified.await;
        }
    }

    /// Returns the API for this docs instance.
    pub fn api(&self) -> &DocsApi {
        &self.api
    }
}

impl std::ops::Deref for Docs {
    type Target = DocsApi;

    fn deref(&self) -> &Self::Target {
        &self.api
    }
}

impl ProtocolHandler for Docs {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        self.engine
            .handle_connection(connection)
            .await
            .map_err(|err| iroh::protocol::AcceptError::from_err(n0_error::anyerr!(err)))?;
        Ok(())
    }

    async fn shutdown(&self) {
        if let Err(err) = Docs::shutdown(self).await {
            tracing::warn!("shutdown error: {:?}", err);
        }
    }
}

/// Builder for the docs protocol.
#[derive(Debug, Default)]
pub struct Builder {
    storage: Storage,
    protect_cb: Option<ProtectCallbackHandler>,
    mutation_gate: Option<Arc<tokio::sync::RwLock<()>>>,
}

impl Builder {
    /// Set the garbage collection protection handler for blobs.
    ///
    /// See [`ProtectCallbackHandler::new`] for details.
    pub fn protect_handler(mut self, protect_handler: ProtectCallbackHandler) -> Self {
        self.protect_cb = Some(protect_handler);
        self
    }

    /// Serialize every inbound set-reconciliation/gossip mutation against an
    /// external collector's whole mark-and-sweep transaction.
    pub fn mutation_gate(mut self, gate: Arc<tokio::sync::RwLock<()>>) -> Self {
        self.mutation_gate = Some(gate);
        self
    }

    /// Build a [`Docs`] protocol given a [`BlobsStore`] and [`Gossip`] protocol.
    pub async fn spawn(
        self,
        endpoint: Endpoint,
        blobs: BlobsStore,
        gossip: Gossip,
    ) -> anyhow::Result<Docs> {
        let replica_store = match &self.storage {
            Storage::Memory => Store::memory(),
            #[cfg(feature = "fs-store")]
            Storage::Persistent(path) => Store::persistent(path.join("docs.redb"))?,
        };
        let author_store = match &self.storage {
            Storage::Memory => DefaultAuthorStorage::Mem,
            #[cfg(feature = "fs-store")]
            Storage::Persistent(path) => {
                DefaultAuthorStorage::Persistent(path.join("default-author"))
            }
        };
        let downloader = blobs.downloader(&endpoint);
        let engine = Engine::spawn(
            endpoint,
            gossip,
            replica_store,
            blobs,
            downloader,
            author_store,
            self.protect_cb,
            self.mutation_gate,
        )
        .await?;
        Ok(Docs::new(engine))
    }
}
