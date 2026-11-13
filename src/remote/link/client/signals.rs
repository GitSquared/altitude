use super::*;

impl LinkClient {
    async fn signal(
        &self,
        peer: impl Into<EndpointAddr>,
        command: SignalCommand,
    ) -> LinkResult<()> {
        match self.say(peer, &LinkRequest::Signal { command }).await? {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn express_input(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
        body: &str,
    ) -> LinkResult<()> {
        match self
            .say(
                peer,
                &LinkRequest::Input {
                    task_id: task_id.into(),
                    body: body.into(),
                },
            )
            .await?
        {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn reply_operator_signal(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
        signal_id: &str,
        body: &str,
    ) -> LinkResult<()> {
        self.signal(
            peer,
            SignalCommand::Reply {
                task_id: task_id.into(),
                signal_id: signal_id.into(),
                body: body.into(),
            },
        )
        .await
    }

    pub async fn resolve_signal(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
        signal_id: &str,
        resolved: bool,
    ) -> LinkResult<()> {
        self.signal(
            peer,
            SignalCommand::Resolve {
                task_id: task_id.into(),
                signal_id: signal_id.into(),
                resolved,
            },
        )
        .await
    }

    pub async fn confirm_operator_signal(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
        signal_id: &str,
    ) -> LinkResult<()> {
        self.signal(
            peer,
            SignalCommand::Accept {
                task_id: task_id.into(),
                signal_id: signal_id.into(),
            },
        )
        .await
    }
    pub async fn put_config(
        &self,
        peer: impl Into<EndpointAddr>,
        bytes: &[u8],
        take_tracker: bool,
    ) -> LinkResult<ConfigPush> {
        let request = LinkRequest::PutConfig {
            hash: BlobHash::of(bytes),
            len: bytes.len() as u64,
            take_tracker,
        };
        match self.say_with_body(peer, &request, bytes).await? {
            LinkResponse::Configured => Ok(ConfigPush::Stored),
            LinkResponse::TrackerLinkedElsewhere {
                provider,
                display_name,
            } => Ok(ConfigPush::TrackerLinkedElsewhere {
                provider,
                display_name,
            }),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }
}
