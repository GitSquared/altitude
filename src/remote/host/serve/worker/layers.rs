use std::{
    env, fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

use iroh::EndpointAddr;
use tokio::fs::File as AsyncFile;

use crate::isolation::HostLayers;
use crate::remote::LinkClient;
use crate::remote::layers::{ArchiveKind, LayerOp, LayerRequest};

// What a host that declares no environment calls itself. Hosts that declare nothing
// share one key, which the 72-hour window bounds: an image that changes without
// saying so is restored the old software until that window passes.
pub(super) const UNDECLARED: &str = "undeclared";

// A host's end of the master's archive store: it asks for an installs layer another
// host built and leaves its own when it had to build it, and does the same with a
// project's cache. Nothing here decides what an archive holds: the engine says which
// layer, and whether it already has it.
#[derive(Clone)]
pub(super) struct LinkLayers {
    client: LinkClient,
    master: EndpointAddr,
    token: String,
    // What this host is, as the image declares it. A layer built on one image is not
    // the software another image's hosts expect, so the store keys on this.
    environment: String,
}

impl LinkLayers {
    pub(super) fn new(
        client: LinkClient,
        master: EndpointAddr,
        token: String,
        environment: String,
    ) -> Self {
        Self {
            client,
            master,
            token,
            environment,
        }
    }

    fn request(&self, kind: ArchiveKind, tag: &str, op: LayerOp) -> LayerRequest {
        LayerRequest {
            token: self.token.clone(),
            kind,
            environment: self.environment.clone(),
            tag: tag.to_owned(),
            op,
        }
    }
}

impl LinkLayers {
    pub(super) async fn fetch_archive(&self, kind: ArchiveKind, tag: &str) -> Option<PathBuf> {
        let path = env::temp_dir().join(format!("altitude-{kind:?}-{tag}.tar"));
        let sink = AsyncFile::create(&path).await.ok()?;
        let request = self.request(kind, tag, LayerOp::Fetch);
        match self
            .client
            .layer_fetch(self.master.clone(), &request, sink)
            .await
        {
            Ok(true) => Some(path),
            Ok(false) => {
                let _ = fs::remove_file(&path);
                None
            }
            // An unreachable master leaves the host to do without, which is what a host
            // did before there was a store at all.
            Err(error) => {
                tracing::warn!(%error, ?kind, %tag, "could not ask the master for an archive");
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    // The open handle is what keeps the archive alive: the caller's temporary file is
    // deleted when it returns, and an unlinked file stays readable through a descriptor
    // already holding it.
    pub(super) async fn send_archive(&self, kind: ArchiveKind, tag: &str, archive: &Path) {
        let Ok(metadata) = fs::metadata(archive) else {
            return;
        };
        let Ok(file) = AsyncFile::open(archive).await else {
            return;
        };
        let bytes = metadata.len();
        let request = self.request(kind, tag, LayerOp::Store { len: bytes });
        match self
            .client
            .layer_store(self.master.clone(), &request, file)
            .await
        {
            Ok(()) => {
                tracing::info!(?kind, %tag, bytes, "left this host's archive for the next one")
            }
            Err(error) => {
                tracing::warn!(%error, ?kind, %tag, "the master did not keep this host's archive")
            }
        }
    }
}

impl HostLayers for LinkLayers {
    fn fetch<'a>(
        &'a self,
        tag: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<PathBuf>> + Send + 'a>> {
        Box::pin(self.fetch_archive(ArchiveKind::Layer, tag))
    }

    fn store<'a>(
        &'a self,
        tag: &'a str,
        archive: PathBuf,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let Ok(file) = AsyncFile::open(&archive).await else {
                return;
            };
            let bytes = file.metadata().await.map(|meta| meta.len()).unwrap_or(0);
            let request = self.request(ArchiveKind::Layer, tag, LayerOp::Store { len: bytes });
            let client = self.client.clone();
            let master = self.master.clone();
            let tag = tag.to_owned();
            // A sandbox start must not wait on an upload that only the next host
            // benefits from. A host torn down mid-upload drops the task, and the
            // next host builds its own layer.
            tokio::spawn(async move {
                match client.layer_store(master, &request, file).await {
                    Ok(()) => {
                        tracing::info!(%tag, bytes, "left this host's layer for the next one")
                    }
                    Err(error) => {
                        tracing::warn!(%error, %tag, "the master did not keep this host's layer")
                    }
                }
            });
        })
    }
}
