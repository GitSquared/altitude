use std::{env, fs, future::Future, path::PathBuf, pin::Pin};

use iroh::EndpointAddr;
use tokio::fs::File as AsyncFile;

use crate::isolation::HostLayers;
use crate::remote::LinkClient;
use crate::remote::layers::{LayerOp, LayerRequest};

// A host's end of the layer store: it asks its master for the machine layer another
// host built, and leaves its own there when it had to build it. Nothing here decides
// what a layer is — the engine says which layer and whether it already has one.
pub(super) struct LayerCache {
    client: LinkClient,
    master: EndpointAddr,
    token: String,
    // What this host is, as the image declares it. A layer built on one image is not
    // the software another image's boxes expect, so the master keys on this.
    environment: String,
}

impl LayerCache {
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

    fn request(&self, tag: &str, op: LayerOp) -> LayerRequest {
        LayerRequest {
            token: self.token.clone(),
            environment: self.environment.clone(),
            tag: tag.to_owned(),
            op,
        }
    }
}

impl HostLayers for LayerCache {
    fn fetch<'a>(
        &'a self,
        tag: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<PathBuf>> + Send + 'a>> {
        Box::pin(async move {
            let path = env::temp_dir().join(format!("altitude-layer-{tag}.tar"));
            let sink = AsyncFile::create(&path).await.ok()?;
            let request = self.request(tag, LayerOp::Fetch);
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
                // An unreachable master leaves the layer to be built here, which is
                // what a host did before there was a store at all.
                Err(error) => {
                    tracing::warn!(%error, %tag, "could not ask the master for a layer");
                    let _ = fs::remove_file(&path);
                    None
                }
            }
        })
    }

    fn store<'a>(
        &'a self,
        tag: &'a str,
        archive: PathBuf,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let Ok(metadata) = fs::metadata(&archive) else {
                return;
            };
            let Ok(file) = AsyncFile::open(&archive).await else {
                return;
            };
            let request = self.request(
                tag,
                LayerOp::Store {
                    len: metadata.len(),
                },
            );
            match self
                .client
                .layer_store(self.master.clone(), &request, file)
                .await
            {
                Ok(()) => tracing::info!(
                    tag,
                    bytes = metadata.len(),
                    "left this host's layer for the next one"
                ),
                Err(error) => {
                    tracing::warn!(%error, %tag, "the master did not keep this host's layer")
                }
            }
        })
    }
}
