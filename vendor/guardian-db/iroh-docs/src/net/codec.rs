use std::future::Future;

use anyhow::{anyhow, ensure};
use bytes::{Buf, BufMut, BytesMut};
use iroh::PublicKey;
use n0_future::SinkExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_stream::StreamExt;
use tokio_util::codec::{Decoder, Encoder, FramedRead, FramedWrite};
use tracing::{debug, trace, Span};

use crate::{
    actor::SyncHandle,
    net::{AbortReason, AcceptError, AcceptOutcome, ConnectError},
    NamespaceId, SyncOutcome,
};

#[derive(Debug, Default)]
struct SyncCodec;

const MAX_MESSAGE_SIZE: usize = 1024 * 1024 * 1024; // This is likely too large, but lets have some restrictions

impl Decoder for SyncCodec {
    type Item = Message;
    type Error = anyhow::Error;
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 4 {
            return Ok(None);
        }
        let bytes: [u8; 4] = src[..4].try_into().unwrap();
        let frame_len = u32::from_be_bytes(bytes) as usize;
        ensure!(
            frame_len <= MAX_MESSAGE_SIZE,
            "received message that is too large: {}",
            frame_len
        );
        if src.len() < 4 + frame_len {
            return Ok(None);
        }

        let message: Message = postcard::from_bytes(&src[4..4 + frame_len])?;
        src.advance(4 + frame_len);
        Ok(Some(message))
    }
}

impl Encoder<Message> for SyncCodec {
    type Error = anyhow::Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let len =
            postcard::serialize_with_flavor(&item, postcard::ser_flavors::Size::default()).unwrap();
        ensure!(
            len <= MAX_MESSAGE_SIZE,
            "attempting to send message that is too large {}",
            len
        );

        dst.put_u32(u32::try_from(len).expect("already checked"));
        if dst.len() < 4 + len {
            dst.resize(4 + len, 0u8);
        }
        postcard::to_slice(&item, &mut dst[4..])?;

        Ok(())
    }
}

/// Sync Protocol
///
/// - Init message: signals which namespace is being synced
/// - N Sync messages
///
/// On any error and on success the substream is closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Message {
    /// Init message (sent by the dialing peer)
    Init {
        /// Namespace to sync
        namespace: NamespaceId,
        /// Initial message
        message: crate::sync::ProtocolMessage,
    },
    /// Sync messages (sent by both peers)
    Sync(crate::sync::ProtocolMessage),
    /// Abort message (sent by the accepting peer to decline a request)
    Abort { reason: AbortReason },
}

/// Runs the initiator side of the sync protocol.
pub(super) async fn run_alice<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    writer: &mut W,
    reader: &mut R,
    handle: &SyncHandle,
    namespace: NamespaceId,
    peer: PublicKey,
) -> Result<SyncOutcome, ConnectError> {
    let peer_bytes = *peer.as_bytes();
    let mut reader = FramedRead::new(reader, SyncCodec);
    let mut writer = FramedWrite::new(writer, SyncCodec);

    let mut progress = Some(SyncOutcome::default());

    // Init message

    let message = handle
        .sync_initial_message(namespace)
        .await
        .map_err(ConnectError::sync)?;
    let init_message = Message::Init { namespace, message };
    trace!("send init message");
    writer
        .send(init_message)
        .await
        .map_err(ConnectError::sync)?;

    // Sync message loop
    while let Some(msg) = reader.next().await {
        let msg = msg.map_err(ConnectError::sync)?;
        match msg {
            Message::Init { .. } => {
                return Err(ConnectError::sync(anyhow!("unexpected init message")));
            }
            Message::Sync(msg) => {
                trace!("recv process message");
                let current_progress = progress.take().unwrap();
                let (reply, next_progress) = handle
                    .sync_process_message(namespace, msg, peer_bytes, current_progress)
                    .await
                    .map_err(ConnectError::sync)?;
                progress = Some(next_progress);
                if let Some(msg) = reply {
                    trace!("send process message");
                    writer
                        .send(Message::Sync(msg))
                        .await
                        .map_err(ConnectError::sync)?;
                } else {
                    break;
                }
            }
            Message::Abort { reason } => {
                return Err(ConnectError::remote_abort(reason));
            }
        }
    }

    trace!("done");
    Ok(progress.unwrap())
}

/// Runs the receiver side of the sync protocol.
#[cfg(test)]
pub(super) async fn run_bob<R, W, F, Fut>(
    writer: &mut W,
    reader: &mut R,
    handle: SyncHandle,
    accept_cb: F,
    peer: PublicKey,
) -> Result<(NamespaceId, SyncOutcome), AcceptError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Fn(NamespaceId, PublicKey) -> Fut,
    Fut: Future<Output = AcceptOutcome>,
{
    let mut state = BobState::new(peer);
    let namespace = state.run(writer, reader, handle, accept_cb).await?;
    Ok((namespace, state.into_outcome()))
}

/// State for the receiver side of the sync protocol.
pub struct BobState {
    namespace: Option<NamespaceId>,
    peer: PublicKey,
    progress: Option<SyncOutcome>,
}

impl BobState {
    /// Create a new state for a single connection.
    pub fn new(peer: PublicKey) -> Self {
        Self {
            peer,
            namespace: None,
            progress: Some(Default::default()),
        }
    }

    fn fail(&self, reason: impl Into<anyhow::Error>) -> AcceptError {
        AcceptError::sync(self.peer, self.namespace(), reason.into())
    }

    /// Handle connection and run to end.
    pub async fn run<R, W, F, Fut>(
        &mut self,
        writer: W,
        reader: R,
        sync: SyncHandle,
        accept_cb: F,
    ) -> Result<NamespaceId, AcceptError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
        F: Fn(NamespaceId, PublicKey) -> Fut,
        Fut: Future<Output = AcceptOutcome>,
    {
        let mut reader = FramedRead::new(reader, SyncCodec);
        let mut writer = FramedWrite::new(writer, SyncCodec);
        while let Some(msg) = reader.next().await {
            let msg = msg.map_err(|e| self.fail(e))?;
            let next = match (msg, self.namespace.as_ref()) {
                (Message::Init { namespace, message }, None) => {
                    Span::current()
                        .record("namespace", tracing::field::display(&namespace.fmt_short()));
                    trace!("recv init message");
                    let accept = accept_cb(namespace, self.peer).await;
                    match accept {
                        AcceptOutcome::Allow => {
                            trace!("allow request");
                        }
                        AcceptOutcome::Reject(reason) => {
                            debug!(?reason, "reject request");
                            writer
                                .send(Message::Abort { reason })
                                .await
                                .map_err(|e| self.fail(e))?;
                            return Err(AcceptError::Abort {
                                namespace,
                                peer: self.peer,
                                reason,
                            });
                        }
                    }
                    let last_progress = self.progress.take().unwrap();
                    let next = sync
                        .sync_process_message(
                            namespace,
                            message,
                            *self.peer.as_bytes(),
                            last_progress,
                        )
                        .await;
                    self.namespace = Some(namespace);
                    next
                }
                (Message::Sync(msg), Some(namespace)) => {
                    trace!("recv process message");
                    let last_progress = self.progress.take().unwrap();
                    sync.sync_process_message(*namespace, msg, *self.peer.as_bytes(), last_progress)
                        .await
                }
                (Message::Init { .. }, Some(_)) => {
                    return Err(self.fail(anyhow!("double init message")));
                }
                (Message::Sync(_), None) => {
                    return Err(self.fail(anyhow!("unexpected sync message before init")));
                }
                (Message::Abort { .. }, _) => {
                    return Err(self.fail(anyhow!("unexpected sync abort message")));
                }
            };
            let (reply, progress) = next.map_err(|e| self.fail(e))?;
            self.progress = Some(progress);
            match reply {
                Some(msg) => {
                    trace!("send process message");
                    writer
                        .send(Message::Sync(msg))
                        .await
                        .map_err(|e| self.fail(e))?;
                }
                None => break,
            }
        }

        trace!("done");

        self.namespace()
            .ok_or_else(|| self.fail(anyhow!("Stream closed before init message")))
    }

    /// Get the namespace that is synced, if available.
    pub fn namespace(&self) -> Option<NamespaceId> {
        self.namespace
    }

    /// Consume self and get the [`SyncOutcome`] for this connection.
    pub fn into_outcome(self) -> SyncOutcome {
        self.progress.unwrap()
    }
}


