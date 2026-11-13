use std::{env, fs, future::Future, path::PathBuf, pin::Pin};

use iroh::EndpointAddr;
use tokio::fs::File as AsyncFile;
use tokio::runtime::Builder;
use tokio::task::spawn_blocking;

use crate::isolation::HostLayers;
use crate::remote::LinkClient;
use crate::remote::layers::{LayerOp, LayerRequest};

// What a host that declares no environment calls itself. Hosts that declare nothing
// share one key, which the 72-hour window bounds: an image that changes without
// saying so is restored the old software until that window passes.
pub(super) const UNDECLARED: &str = "undeclared";

// A host's end of the layer store: it asks its master for an installs layer another
// host built, and leaves its own there when it had to build it. Nothing here decides
// what a layer is: the engine says which one, and whether it already has it.
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

    fn request(&self, tag: &str, op: LayerOp) -> LayerRequest {
        LayerRequest {
            token: self.token.clone(),
            environment: self.environment.clone(),
            tag: tag.to_owned(),
            op,
        }
    }
}

impl HostLayers for LinkLayers {
    fn fetch<'a>(
        &'a self,
        tag: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<PathBuf>> + Send + 'a>> {
        Box::pin(async move {
            let path = env::temp_dir().join(format!("altitude-layer-{tag}.tar"));
            let sink = AsyncFile::create(&path).await.ok()?;
            let request = self.request(tag, LayerOp::Fetch);
            // This runs inside the call that asked for the sandbox, and the master is
            // waiting on that call's answer: a dial from here to the machine that is
            // waiting on us never completes, and both calls die of the silence. A thread
            // of its own is what takes the dial off the task being awaited.
            let client = self.client.clone();
            let master = self.master.clone();
            let answer = spawn_blocking(move || {
                let runtime = Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                runtime.block_on(client.layer_fetch(master, &request, sink))
            })
            .await;
            let answer = match answer {
                Ok(answer) => answer,
                Err(error) => Err(error.to_string().into()),
            };
            match answer {
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
