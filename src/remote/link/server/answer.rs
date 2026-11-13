use super::*;

pub(super) async fn answer(
    handler: &LinkHandler,
    peer: EndpointId,
    mut send: SendStream,
    mut recv: RecvStream,
) -> LinkResult<()> {
    let request: LinkRequest = read_frame(&mut recv).await?;
    match request {
        LinkRequest::Identify => {
            let response = LinkResponse::Identified {
                remote: handler.id.to_string(),
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::RunTask { hash, len } => {
            let bytes = read_body(&mut recv, len, MAX_BLOB_BYTES).await?;
            if BlobHash::of(&bytes) != hash {
                write_frame(
                    &mut send,
                    &LinkResponse::Rejected {
                        reason: "task request digest did not match".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            let request = serde_json::from_slice(&bytes)?;
            let response = match handler.tasks.run(peer, request).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::WatchTasks => {
            let watching = match handler.tasks.watch(peer).await {
                Ok(watching) => watching,
                Err(reason) => {
                    write_frame(&mut send, &LinkResponse::Rejected { reason }).await?;
                    return Ok(());
                }
            };
            // The snapshot is written before the changes queued behind it are
            // drained, so what a watch opens with is never older than what follows.
            write_frame(
                &mut send,
                &LinkResponse::Tasks {
                    summaries: watching.snapshot,
                },
            )
            .await?;
            let mut changes = watching.changes;
            while let Some(change) = changes.recv().await {
                let response = match change {
                    WatchEvent::Changed(summary) => LinkResponse::TaskChanged { summary },
                    WatchEvent::Removed(id) => LinkResponse::TaskRemoved { id },
                    WatchEvent::Ready => LinkResponse::Ready,
                    WatchEvent::Snapshot(_) => continue,
                };
                write_frame(&mut send, &response).await?;
            }
            return Ok(());
        }
        LinkRequest::ReadPicture { id } => {
            let store = attachments_dir(&handler.data_dir);
            let response = if !picture::is_id(&id) {
                LinkResponse::Rejected {
                    reason: "that is not a picture id".into(),
                }
            } else {
                match async_fs::read(picture::path(&store, &id)).await {
                    Ok(bytes) if bytes.len() as u64 <= picture::LARGEST => {
                        write_frame(
                            &mut send,
                            &LinkResponse::Picture {
                                id: id.clone(),
                                len: bytes.len() as u64,
                            },
                        )
                        .await?;
                        send.write_all(&bytes).await?;
                        return Ok(());
                    }
                    Ok(_) => LinkResponse::Rejected {
                        reason: "that picture is larger than a picture may be".into(),
                    },
                    Err(_) => LinkResponse::Rejected {
                        reason: "this machine does not have that picture".into(),
                    },
                }
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::ListSignals { task_id } => {
            let response = match handler.tasks.signal_threads(peer, task_id).await {
                Ok(signals) => LinkResponse::Signals { signals },
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::Signal { command } => {
            let response = match handler.tasks.signal(peer, command).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::Input { task_id, body } => {
            let response = match handler.tasks.input(peer, task_id, body).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::AbandonTake { transfer_id } => {
            let response = match handler.tasks.abandon_take(peer, transfer_id).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::AbandonImport { transfer_id } => {
            let response = match handler.tasks.abandon_import(peer, transfer_id).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::RemoveTask { task_id } => {
            let response = match handler.tasks.remove(peer, task_id).await {
                Ok(()) => LinkResponse::Removed,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::TakeProgress { task_id } => {
            let response = match handler.tasks.take_progress(&task_id) {
                Some(progress) => LinkResponse::Collecting {
                    stage: progress.stage,
                    done: progress.done,
                    total: progress.total,
                    unit: match progress.unit {
                        Unit::Bytes => "bytes".to_owned(),
                        Unit::Files => "files".to_owned(),
                    },
                },
                None => LinkResponse::Rejected {
                    reason: "nothing is being collected for that task".to_owned(),
                },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::TakeTask { task_id } => {
            let response = match handler.tasks.hand_over(peer, task_id).await {
                Ok(handover) => LinkResponse::Handover {
                    handover: Box::new(handover),
                },
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::FetchTransferPart {
            transfer_id,
            part,
            offset,
            len,
        } => {
            match handler
                .tasks
                .transfer_piece(peer, transfer_id, part, offset, len)
                .await
            {
                Ok(bytes) => {
                    let announced = bytes.len() as u64;
                    write_frame(&mut send, &LinkResponse::TransferPiece { len: announced }).await?;
                    send.write_all(&bytes).await?;
                }
                Err(reason) => write_frame(&mut send, &LinkResponse::Rejected { reason }).await?,
            }
        }
        LinkRequest::HandedOver { transfer_id } => {
            let response = match handler.tasks.handed_over(peer, transfer_id).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::PrepareImport { handover } => {
            let response = match handler.tasks.prepare_import(peer, *handover).await {
                Ok(()) => LinkResponse::ImportPrepared,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::PutImportPart {
            transfer_id,
            part,
            offset,
            hash,
            len,
        } => {
            let response = if len == 0 || len > TRANSFER_PIECE_BYTES {
                LinkResponse::Rejected {
                    reason: "invalid transfer piece length".into(),
                }
            } else {
                let bytes = read_body(&mut recv, len, TRANSFER_PIECE_BYTES).await?;
                if BlobHash::of(&bytes) != hash {
                    LinkResponse::Rejected {
                        reason: "transfer piece digest did not match".into(),
                    }
                } else {
                    match handler
                        .tasks
                        .put_import_part(peer, transfer_id, part, offset, bytes)
                        .await
                    {
                        Ok(()) => LinkResponse::Accepted,
                        Err(reason) => LinkResponse::Rejected { reason },
                    }
                }
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::StageImport { transfer_id } => {
            let response = match handler.tasks.stage_import(peer, transfer_id).await {
                Ok(task_id) => LinkResponse::Imported { task_id },
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::CommitImport { transfer_id } => {
            let response = match handler.tasks.commit_import(peer, transfer_id).await {
                Ok(()) => LinkResponse::Accepted,
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::ListTasks => {
            let response = match handler.tasks.list(peer).await {
                Ok(summaries) => LinkResponse::Tasks { summaries },
                Err(reason) => LinkResponse::Rejected { reason },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::HeldProfiles => {
            let response = LinkResponse::HeldProfiles {
                profiles: handler.held_profiles(peer),
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::PutConfig {
            hash,
            len,
            take_tracker,
        } => {
            let bytes = read_body(&mut recv, len, MAX_BLOB_BYTES).await?;
            let actual = BlobHash::of(&bytes);
            let response = if actual != hash {
                LinkResponse::Rejected {
                    reason: format!("digest mismatch: announced {hash}, received {actual}"),
                }
            } else {
                let _writing = handler.config_lock.lock().await;
                match handler.write_config(peer, &bytes, take_tracker) {
                    Ok(ConfigWrite::Written { forget_logins }) => {
                        match handler.tasks.configure(peer, forget_logins).await {
                            Ok(()) => LinkResponse::Configured,
                            Err(reason) => LinkResponse::Rejected { reason },
                        }
                    }
                    Ok(ConfigWrite::LinkedElsewhere {
                        provider,
                        display_name,
                    }) => LinkResponse::TrackerLinkedElsewhere {
                        provider,
                        display_name,
                    },
                    Err(error) => LinkResponse::Rejected {
                        reason: format!("could not take the configuration: {error}"),
                    },
                }
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::RegisterWorker {
            token,
            name,
            capacity,
            addr,
        } => {
            let response = match &handler.workers {
                Some(workers) => match workers.register_worker(addr, token, name, capacity).await {
                    Ok(()) => LinkResponse::Accepted,
                    Err(reason) => LinkResponse::Rejected { reason },
                },
                None => LinkResponse::Rejected {
                    reason: "this machine does not take isolation workers".into(),
                },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::WorkerHeartbeat { name } => {
            let response = match &handler.workers {
                Some(workers) => match workers.worker_heartbeat(peer, name).await {
                    Ok(()) => LinkResponse::Accepted,
                    Err(reason) => LinkResponse::Rejected { reason },
                },
                None => LinkResponse::Rejected {
                    reason: "this machine does not take isolation workers".into(),
                },
            };
            write_frame(&mut send, &response).await?;
        }
        LinkRequest::AgentChannel { socket, token } => {
            let Some(access) = &handler.agent_channels else {
                write_frame(
                    &mut send,
                    &LinkResponse::Rejected {
                        reason: "this machine serves no agent channels".into(),
                    },
                )
                .await?;
                return Ok(());
            };
            let path = Path::new(&socket);
            let allowed = token == access.token
                && path.starts_with(&access.directory)
                && path.extension().and_then(|kind| kind.to_str()) == Some("sock");
            let channel = if allowed {
                UnixStream::connect(path).await.ok()
            } else {
                None
            };
            let Some(channel) = channel else {
                write_frame(
                    &mut send,
                    &LinkResponse::Rejected {
                        reason: "that is not a channel of this machine".into(),
                    },
                )
                .await?;
                return Ok(());
            };
            write_frame(&mut send, &LinkResponse::Accepted).await?;
            // Two directions, until one side is done; the worker keeps the
            // sandbox's connection open for the whole agent session.
            let (mut reader, mut writer) = channel.into_split();
            let upstream = async {
                let _ = tokio_io::copy(&mut recv, &mut writer).await;
                let _ = writer.shutdown().await;
            };
            let downstream = async {
                let _ = tokio_io::copy(&mut reader, &mut send).await;
            };
            tokio::join!(upstream, downstream);
            return Ok(());
        }
    }
    send.finish()?;
    Ok(())
}
