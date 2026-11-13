use super::*;

impl LinkClient {
    pub async fn watch_tasks(
        &self,
        peer: impl Into<EndpointAddr>,
        events: mpsc::UnboundedSender<WatchEvent>,
    ) -> LinkResult<()> {
        let dial = self.dial(peer, Conversation::Link).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, &LinkRequest::WatchTasks).await?;
        send.finish()?;
        loop {
            let answer: LinkResponse = read_frame(&mut recv).await?;
            let event = match answer {
                LinkResponse::Tasks { summaries } => WatchEvent::Snapshot(summaries),
                LinkResponse::TaskChanged { summary } => WatchEvent::Changed(summary),
                LinkResponse::TaskRemoved { id } => WatchEvent::Removed(id),
                LinkResponse::Ready => WatchEvent::Ready,
                LinkResponse::Rejected { reason } => return Err(reason.into()),
                other => return Err(format!("unexpected answer: {other:?}").into()),
            };
            if events.send(event).is_err() {
                // Nobody is reading: the watch ends with its reader.
                return Ok(());
            }
        }
    }

    pub async fn remove_task(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
    ) -> LinkResult<()> {
        match self
            .say(
                peer,
                &LinkRequest::RemoveTask {
                    task_id: task_id.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Removed => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn held_profiles(
        &self,
        peer: impl Into<EndpointAddr>,
    ) -> LinkResult<Vec<ProfileDigest>> {
        match self.say(peer, &LinkRequest::HeldProfiles).await? {
            LinkResponse::HeldProfiles { profiles } => Ok(profiles),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn prepare_take(&self, peer: &EndpointAddr, task_id: &str) -> LinkResult<Handover> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::TakeTask {
                    task_id: task_id.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Handover { handover } => Ok(*handover),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn take_progress(
        &self,
        peer: &EndpointAddr,
        task_id: &str,
    ) -> Option<TransferUpdate> {
        let answer = self
            .say(
                peer.clone(),
                &LinkRequest::TakeProgress {
                    task_id: task_id.to_owned(),
                },
            )
            .await
            .ok()?;
        match answer {
            LinkResponse::Collecting {
                stage,
                done,
                total,
                unit,
            } => Some(TransferUpdate {
                stage,
                done,
                total,
                unit: if unit == "files" {
                    Unit::Files
                } else {
                    Unit::Bytes
                },
            }),
            _ => None,
        }
    }

    pub async fn fetch_part(
        &self,
        peer: &EndpointAddr,
        transfer_id: &str,
        part: TransferPart,
        archive: &TransferArchive,
        landing: &Path,
        transfer: &Transfer,
    ) -> LinkResult<()> {
        if let Some(parent) = landing.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(landing)?;
        let mut hasher = blake3::Hasher::new();
        let mut offset = 0;
        while offset < archive.len {
            if transfer.is_stopped() {
                return Err(STOPPED.into());
            }
            let len = TRANSFER_PIECE_BYTES.min(archive.len - offset);
            let bytes = self
                .ask_for_body(
                    peer.clone(),
                    &LinkRequest::FetchTransferPart {
                        transfer_id: transfer_id.to_owned(),
                        part,
                        offset,
                        len,
                    },
                )
                .await?;
            if bytes.len() as u64 != len {
                return Err("the transfer piece was truncated".into());
            }
            hasher.update(&bytes);
            file.write_all(&bytes)?;
            offset += len;
            transfer.advanced(len);
        }
        file.sync_all()?;
        let arrived = hasher.finalize().to_hex().to_string();
        if arrived != archive.digest {
            return Err(format!("archive digest mismatch: {arrived}").into());
        }
        tracing::info!(%transfer_id, ?part, bytes = offset, "fetched a transfer archive");
        Ok(())
    }

    pub async fn abandon_take(&self, peer: &EndpointAddr, transfer_id: &str) -> LinkResult<()> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::AbandonTake {
                    transfer_id: transfer_id.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn abandon_import(&self, peer: &EndpointAddr, transfer_id: &str) -> LinkResult<()> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::AbandonImport {
                    transfer_id: transfer_id.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn confirm_take(&self, peer: &EndpointAddr, transfer_id: &str) -> LinkResult<()> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::HandedOver {
                    transfer_id: transfer_id.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn prepare_import(&self, peer: &EndpointAddr, handover: &Handover) -> LinkResult<()> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::PrepareImport {
                    handover: Box::new(handover.clone()),
                },
            )
            .await?
        {
            LinkResponse::ImportPrepared => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn put_import_part(
        &self,
        peer: &EndpointAddr,
        transfer_id: &str,
        part: TransferPart,
        offset: u64,
        bytes: &[u8],
    ) -> LinkResult<()> {
        let request = LinkRequest::PutImportPart {
            transfer_id: transfer_id.to_owned(),
            part,
            offset,
            hash: BlobHash::of(bytes),
            len: bytes.len() as u64,
        };
        match self.say_with_body(peer.clone(), &request, bytes).await? {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn stage_import(&self, peer: &EndpointAddr, transfer_id: &str) -> LinkResult<String> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::StageImport {
                    transfer_id: transfer_id.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Imported { task_id } => Ok(task_id),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn commit_import(&self, peer: &EndpointAddr, transfer_id: &str) -> LinkResult<()> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::CommitImport {
                    transfer_id: transfer_id.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn run_task(
        &self,
        peer: impl Into<EndpointAddr>,
        request: TaskRequest,
    ) -> LinkResult<()> {
        let bytes = serde_json::to_vec(&request)?;
        let request = LinkRequest::RunTask {
            hash: BlobHash::of(&bytes),
            len: bytes.len() as u64,
        };
        match self.say_with_body(peer, &request, &bytes).await? {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    // The bytes a digest names, read from the machine that holds them.
    pub async fn picture(&self, peer: impl Into<EndpointAddr>, id: &str) -> LinkResult<Vec<u8>> {
        let dial = self.dial(peer, Conversation::Link).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, &LinkRequest::ReadPicture { id: id.to_owned() }).await?;
        send.finish()?;
        match read_frame::<_, LinkResponse>(&mut recv).await? {
            LinkResponse::Picture { id: answer, len } if answer == id => {
                read_body(&mut recv, len, picture::LARGEST).await
            }
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn signal_threads(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
    ) -> LinkResult<Vec<TaskAsk>> {
        match self
            .say(
                peer,
                &LinkRequest::ListSignals {
                    task_id: task_id.into(),
                },
            )
            .await?
        {
            LinkResponse::Signals { signals } => Ok(signals),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }
}
