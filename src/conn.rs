use std::collections::VecDeque;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::ready;

use async_tungstenite::tungstenite::Message as WsMessage;
use async_tungstenite::{tungstenite::protocol::WebSocketConfig, WebSocketStream};
use futures::stream::Stream;
use futures::task::{Context, Poll};
use futures::{SinkExt, StreamExt};

use chromiumoxide_cdp::cdp::browser_protocol::target::SessionId;
use chromiumoxide_types::{CallId, EventMessage, Message, MethodCall, MethodId};

use crate::error::CdpError;
use crate::error::Result;

cfg_if::cfg_if! {
    if #[cfg(feature = "async-std-runtime")] {
       use async_tungstenite::async_std::ConnectStream;
    } else if #[cfg(feature = "tokio-runtime")] {
        use async_tungstenite::tokio::ConnectStream;
    }
}
/// Exchanges the messages with the websocket
#[must_use = "streams do nothing unless polled"]
#[derive(Debug)]
pub struct Connection<T: EventMessage> {
    /// Queue of commands to send.
    pending_commands: VecDeque<MethodCall>,
    /// The websocket of the chromium instance
    ws: WebSocketStream<ConnectStream>,
    /// The identifier for a specific command
    next_id: usize,
    needs_flush: bool,
    /// The message that is currently being proceessed
    pending_flush: Option<MethodCall>,
    _marker: PhantomData<T>,
}

impl<T: EventMessage + Unpin> Connection<T> {
    pub async fn connect(debug_ws_url: impl AsRef<str>) -> Result<Self> {
        let config = WebSocketConfig::default();

        cfg_if::cfg_if! {
            if #[cfg(feature = "async-std-runtime")] {
               let (ws, _) = async_tungstenite::async_std::connect_async_with_config(debug_ws_url.as_ref(), Some(config)).await?;
            } else if #[cfg(feature = "tokio-runtime")] {
                 let (ws, _) = async_tungstenite::tokio::connect_async_with_config(debug_ws_url.as_ref(), Some(config)).await?;
            }
        }

        Ok(Self {
            pending_commands: Default::default(),
            ws,
            next_id: 0,
            needs_flush: false,
            pending_flush: None,
            _marker: Default::default(),
        })
    }
}

impl<T: EventMessage> Connection<T> {
    fn next_call_id(&mut self) -> CallId {
        let id = CallId::new(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// Queue in the command to send over the socket and return the id for this
    /// command
    pub fn submit_command(
        &mut self,
        method: MethodId,
        session_id: Option<SessionId>,
        params: serde_json::Value,
    ) -> serde_json::Result<CallId> {
        let id = self.next_call_id();
        let call = MethodCall {
            id,
            method,
            session_id: session_id.map(Into::into),
            params,
        };
        self.pending_commands.push_back(call);
        Ok(id)
    }

    /// flush any processed message and start sending the next over the conn
    /// sink
    fn start_send_next(&mut self, cx: &mut Context<'_>) -> Result<()> {
        if self.needs_flush {
            if let Poll::Ready(Ok(())) = self.ws.poll_flush_unpin(cx) {
                self.needs_flush = false;
            }
        }
        if self.pending_flush.is_none() && !self.needs_flush {
            if let Some(cmd) = self.pending_commands.pop_front() {
                tracing::trace!("Sending {:?}", cmd);
                let msg = serde_json::to_string(&cmd)?;
                self.ws.start_send_unpin(msg.into())?;
                self.pending_flush = Some(cmd);
            }
        }
        Ok(())
    }
}

impl<T: EventMessage + Unpin> Stream for Connection<T> {
    type Item = Result<Message<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        loop {
            loop {
                // queue in the next message if not currently flushing
                if let Err(err) = pin.start_send_next(cx) {
                    return Poll::Ready(Some(Err(err)));
                }

                // send the message
                if let Some(call) = pin.pending_flush.take() {
                    if pin.ws.poll_ready_unpin(cx).is_ready() {
                        pin.needs_flush = true;
                        // try another flush
                        continue;
                    } else {
                        pin.pending_flush = Some(call);
                    }
                }

                break;
            }

            // read from the ws
            match ready!(pin.ws.poll_next_unpin(cx)) {
                Some(Ok(WsMessage::Text(text))) => {
                    let ready = match serde_json::from_str::<Message<T>>(&text) {
                        Ok(msg) => {
                            tracing::trace!("Received {:?}", msg);
                            Ok(msg)
                        }
                        Err(err) => {
                            tracing::debug!(target: "chromiumoxide::conn::raw_ws::parse_errors", msg = %text, "Failed to parse raw WS message");
                            tracing::error!("Failed to deserialize WS response {}", err);
                            // Go to the next iteration and try reading the next message
                            // in the hopes we can reconver and continue working.
                            continue;
                        }
                    };
                    return Poll::Ready(Some(ready));
                }
                Some(Ok(WsMessage::Close(_))) => return Poll::Ready(None),
                // ignore ping and pong
                Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Pong(_))) => {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Some(Ok(msg)) => return Poll::Ready(Some(Err(CdpError::UnexpectedWsMessage(msg)))),
                Some(Err(err)) => return Poll::Ready(Some(Err(CdpError::Ws(err)))),
                None => {
                    // ws connection closed
                    return Poll::Ready(None);
                }
            }
        }
    }
}
