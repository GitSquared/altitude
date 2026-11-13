use super::*;

impl LinkClient {
    // One cache call to a master, carrying the body a write needs and returning the
    // bytes a read produced.
    #[cfg(target_os = "linux")]
    pub(crate) async fn cache_call(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &CacheRequest,
        body: &[u8],
    ) -> LinkResult<(CacheReply, Vec<u8>)> {
        // The cache answers on its own ALPN, not the link's: a master serves three
        // conversations, and a host that dials the wrong one is refused at the
        // handshake rather than told to try again.
        let dial = self.dial(peer, Conversation::Cache).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        if !body.is_empty() {
            send.write_all(body).await?;
        }
        send.finish()?;
        let reply: CacheReply = read_frame(&mut recv).await?;
        let bytes = match &reply {
            CacheReply::Data { len } => {
                read_body(&mut recv, u64::from(*len), MAX_BLOB_BYTES).await?
            }
            _ => Vec::new(),
        };
        Ok((reply, bytes))
    }

    // What another host built, fetched by this one: the archive is copied into the
    // sink, and `false` says the master has nothing for this environment and tag.
    pub(crate) async fn layer_fetch(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LayerRequest,
        mut sink: AsyncFile,
    ) -> LinkResult<bool> {
        let dial = self.dial(peer, Conversation::Layers).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish()?;
        match read_frame::<_, LayerReply>(&mut recv).await? {
            LayerReply::Blob { .. } => {
                copy(&mut recv, &mut sink).await?;
                sink.sync_all().await?;
                Ok(true)
            }
            LayerReply::Missing => Ok(false),
            LayerReply::Failed(reason) => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    // This host's copy, left for the next one. A failure is the caller's to log and
    // forget: a layer that cannot be shared is a layer the next host builds itself.
    pub(crate) async fn layer_store(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LayerRequest,
        mut archive: AsyncFile,
    ) -> LinkResult<()> {
        let dial = self.dial(peer, Conversation::Layers).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        copy(&mut archive, &mut send).await?;
        send.finish()?;
        match read_frame::<_, LayerReply>(&mut recv).await? {
            LayerReply::Stored => Ok(()),
            LayerReply::Failed(reason) => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    // One sandbox call to a worker the master provisioned.
    pub(crate) async fn worker_call(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
    ) -> LinkResult<WorkerReply> {
        let dial = self.dial(peer, Conversation::Worker).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish()?;
        read_frame(&mut recv).await
    }

    // A provision carries the operator's profiles, which are too large for a frame:
    // they follow it as the rest of the stream, the way a delivered file does.
    pub(crate) async fn worker_provision(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
        body: &[u8],
    ) -> LinkResult<WorkerReply> {
        let dial = self.dial(peer, Conversation::Worker).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.write_all(body).await?;
        send.finish()?;
        read_frame(&mut recv).await
    }

    // A host carries one connection from an agent inside its sandbox to the
    // channel the master serves for that task.
    pub(crate) async fn pipe_agent_channel(
        &self,
        peer: impl Into<EndpointAddr>,
        socket: &str,
        token: &str,
        tcp: TcpStream,
    ) -> LinkResult<()> {
        let dial = self.dial(peer, Conversation::Link).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(
            &mut send,
            &LinkRequest::AgentChannel {
                socket: socket.to_owned(),
                token: token.to_owned(),
            },
        )
        .await?;
        match read_frame::<_, LinkResponse>(&mut recv).await? {
            LinkResponse::Accepted => {}
            LinkResponse::Rejected { reason } => return Err(reason.into()),
            other => return Err(format!("unexpected answer: {other:?}").into()),
        }
        let (mut box_read, mut box_write) = tcp.into_split();
        let to_master = async {
            let _ = copy(&mut box_read, &mut send).await;
            let _ = send.finish();
        };
        let to_box = async {
            let _ = copy(&mut recv, &mut box_write).await;
        };
        tokio::join!(to_master, to_box);
        Ok(())
    }

    // Send a file's bytes to a worker on the stream that carries the call.
    pub(crate) async fn worker_deliver(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
        mut file: AsyncFile,
    ) -> LinkResult<()> {
        let dial = self.dial(peer, Conversation::Worker).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        copy(&mut file, &mut send).await?;
        send.finish()?;
        match read_frame::<_, WorkerReply>(&mut recv).await? {
            WorkerReply::Ok => Ok(()),
            WorkerReply::Failed(reason) => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    // Receive a collected file or tree on the stream the call opened.
    pub(crate) async fn worker_collect(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
        mut sink: AsyncFile,
    ) -> LinkResult<(u64, u64)> {
        let dial = self.dial(peer, Conversation::Worker).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish()?;
        match read_frame::<_, WorkerReply>(&mut recv).await? {
            WorkerReply::Blob { files, bytes, .. } => {
                copy(&mut recv, &mut sink).await?;
                Ok((files, bytes))
            }
            WorkerReply::Failed(reason) => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    // A command on a worker keeps its stream open: input goes one way and events
    // the other until the command exits.
    pub(crate) async fn worker_command(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
    ) -> LinkResult<CommandSession> {
        let dial = self.dial(peer, Conversation::Worker).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        match read_frame::<_, WorkerReply>(&mut recv).await? {
            WorkerReply::Ready => {}
            WorkerReply::Failed(reason) => return Err(reason.into()),
            other => return Err(format!("unexpected answer: {other:?}").into()),
        }
        let (commands, mut wanted) = mpsc::unbounded_channel();
        let (producing, events) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            // The connection lives as long as the command does.
            let _dial = dial;
            loop {
                tokio::select! {
                    command = wanted.recv() => {
                        let Some(command) = command else {
                            break;
                        };
                        if write_frame(&mut send, &CommandFrame::Drive(command)).await.is_err() {
                            break;
                        }
                    }
                    frame = read_frame::<_, CommandFrame>(&mut recv) => {
                        match frame {
                            Ok(CommandFrame::Report(event)) => {
                                let done = matches!(event, CommandEvent::Exited(_) | CommandEvent::Failed(_));
                                if producing.send(event).is_err() {
                                    break;
                                }
                                if done {
                                    break;
                                }
                            }
                            _ => break,
                        }
                    }
                }
            }
            let _ = send.finish();
        });
        Ok(CommandSession { commands, events })
    }

    // A provisioned worker tying itself to the master it was made for. The token
    // is the only thing that says the connection is wanted.
    pub async fn register_worker(
        &self,
        peer: impl Into<EndpointAddr>,
        addr: EndpointAddr,
        token: &str,
        name: &str,
        capacity: WorkerReport,
    ) -> LinkResult<()> {
        match self
            .say(
                peer,
                &LinkRequest::RegisterWorker {
                    token: token.to_owned(),
                    name: name.to_owned(),
                    capacity,
                    addr,
                },
            )
            .await?
        {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn worker_heartbeat(
        &self,
        peer: impl Into<EndpointAddr>,
        name: &str,
    ) -> LinkResult<()> {
        match self
            .say(
                peer,
                &LinkRequest::WorkerHeartbeat {
                    name: name.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn shutdown(&self) {
        self.endpoint.close().await;
    }
}
