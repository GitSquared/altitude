use tokio::{io::AsyncReadExt, time};

use super::*;

impl LinkClient {
    // What another host left, fetched by this one: the archive is copied into the sink,
    // and `false` says the master has nothing for this kind, environment and tag.
    // An archive is an optimisation and a host that cannot fetch one does without, so the
    // fetch is bounded: a conversation that stalls costs this host a build or a cold
    // start, not the ten minutes a connection's idle window takes to end it.
    pub(crate) async fn archive_fetch(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &ArchiveRequest,
        mut sink: AsyncFile,
    ) -> LinkResult<bool> {
        let peer = peer.into();
        let patience = request.kind.fetch_patience();
        match time::timeout(patience, self.download_archive(peer, request, &mut sink)).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    ?patience,
                    kind = ?request.kind,
                    "asking the master for an archive timed out, dial included; this host does without"
                );
                Ok(false)
            }
        }
    }

    async fn download_archive(
        &self,
        peer: EndpointAddr,
        request: &ArchiveRequest,
        sink: &mut AsyncFile,
    ) -> LinkResult<bool> {
        let dial = self.dial(peer, Conversation::Layers).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish()?;
        match read_frame::<_, ArchiveReply>(&mut recv).await? {
            ArchiveReply::Blob { .. } => {
                copy(&mut recv, sink).await?;
                sink.sync_all().await?;
                Ok(true)
            }
            ArchiveReply::Missing => Ok(false),
            ArchiveReply::Failed(reason) => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    // This host's copy, left for the next one. A failure is the caller's to log and
    // forget: an archive that cannot be shared is one the next host does without.
    // Bounded as a whole, dial and upload and answer: nothing else ends a conversation
    // whose peer stays connected and says nothing, short of the connection's idle window.
    pub(crate) async fn archive_store(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &ArchiveRequest,
        mut archive: AsyncFile,
    ) -> LinkResult<()> {
        let peer = peer.into();
        let patience = request.kind.store_patience();
        let stored = time::timeout(patience, async {
            let dial = self.dial(peer, Conversation::Layers).await?;
            let (mut send, mut recv) = dial.0.open_bi().await?;
            let sent: LinkResult<()> = async {
                write_frame(&mut send, request).await?;
                copy(&mut archive, &mut send).await?;
                send.finish()?;
                Ok(())
            }
            .await;
            // A master that refuses a layer answers before it has read the archive and
            // stops the stream, so the write fails with "stopped by peer" while the
            // reason it stopped is already waiting in the reply.
            match read_frame::<_, ArchiveReply>(&mut recv).await {
                Ok(ArchiveReply::Failed(reason)) => {
                    Err(format!("the master refused the archive: {reason}").into())
                }
                Ok(ArchiveReply::Stored) => sent,
                Ok(other) => Err(format!("unexpected answer: {other:?}").into()),
                Err(error) => sent.and(Err(error)),
            }
        })
        .await;
        match stored {
            Ok(result) => result,
            Err(_) => Err(format!("storing an archive took longer than {patience:?}").into()),
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
        // A host that takes long enough to answer says what it is doing first, and that
        // narration belongs to any call, not only to a provision: the answer is the first
        // frame that is not one of those.
        loop {
            let frame = read_frame::<_, WorkerReply>(&mut recv).await?;
            if !frame.is_narration() {
                return Ok(frame);
            }
        }
    }

    // A provision carries the operator's profiles, which are too large for a frame:
    // they follow it as the rest of the stream, the way a delivered file does.
    pub(crate) async fn worker_provision(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
        body: &[u8],
        mut on_side: impl FnMut(WorkerReply),
    ) -> LinkResult<WorkerReply> {
        let dial = self.dial(peer, Conversation::Worker).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.write_all(body).await?;
        send.finish()?;
        // A host narrates what it is doing while it does it, so a slow provision reads
        // as work in progress rather than as a call that never answered.
        loop {
            let frame = read_frame::<_, WorkerReply>(&mut recv).await?;
            if !frame.is_narration() {
                return Ok(frame);
            }
            on_side(frame);
        }
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

    // Receive a collected file or tree on the stream the call opened. The sink is the
    // caller's, so a caller that must not let go of the source before the copy is on
    // its disk can flush it before this returns.
    pub(crate) async fn worker_collect(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
        sink: &mut AsyncFile,
    ) -> LinkResult<(u64, u64)> {
        let dial = self.dial(peer, Conversation::Worker).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish()?;
        match read_frame::<_, WorkerReply>(&mut recv).await? {
            // The frame says how many bytes follow, and that is the whole account of
            // completeness there is: a stream that ended early looks exactly like one
            // that ended, so what arrives is counted against the length declared.
            WorkerReply::Blob { len, files, bytes } => {
                let copied = copy(&mut recv.take(len), sink).await?;
                if copied != len {
                    return Err(
                        format!("{copied} of {len} bytes of a worker's blob arrived").into(),
                    );
                }
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
