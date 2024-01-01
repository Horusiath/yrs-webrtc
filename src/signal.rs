use crate::room::Room;
use crate::{PeerId, Result, SignalingConn};
use arc_swap::ArcSwapOption;
use futures_util::stream::SplitSink;
use futures_util::{ready, SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::net::TcpStream;
use tokio::pin;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

/// Signaling connection, which uses WebSockets side lane connection to drive group subscription
/// mechanism and signal exchange used for WebRTC offer/answer cycle.
#[derive(Debug)]
pub struct WSSignalingConn {
    url: Arc<str>,
    broadcast: tokio::sync::broadcast::Sender<Arc<Message<'static>>>,
    msg_handler: ArcSwapOption<JoinHandle<Result<()>>>,
    sender: Mutex<WsSink>,
}

impl WSSignalingConn {
    pub async fn connect<U: Into<Arc<str>>>(url: U) -> Result<Self> {
        let url = url.into();
        let parsed_url = Url::parse(&url)?;
        let (ws, _response) = tokio_tungstenite::connect_async(parsed_url).await?;
        let (sender, mut receiver) = ws.split();
        let (broadcast, _) = tokio::sync::broadcast::channel(10);
        let broadcast_sender = broadcast.clone();
        let msg_handler: JoinHandle<Result<()>> = tokio::spawn(async move {
            while let Some(msg) = receiver.next().await {
                let msg = msg?;
                if msg.is_text() || msg.is_binary() {
                    let parsed: Message = serde_json::from_slice(&msg.into_data())?;
                    log::trace!("signalling conn received: {:?}", parsed);
                    if let Message::Publish { .. } = parsed {
                        let msg = Arc::new(parsed);
                        broadcast_sender.send(msg)?;
                    }
                }
            }
            Ok(())
        });

        Ok(WSSignalingConn {
            url,
            broadcast,
            msg_handler: ArcSwapOption::new(Some(Arc::new(msg_handler))),
            sender: Mutex::new(sender),
        })
    }

    /// Returns an URL of the server this [WSSignalingConn] has been connected to.
    pub fn url(&self) -> &Arc<str> {
        &self.url
    }

    /// Checks if current [SignalingConn] has been closed gracefully.
    pub fn is_closed(&self) -> bool {
        self.msg_handler.load().is_none()
    }

    /// Sends a [Message] over current connection to the WebSocket server.
    pub async fn send<'a>(&self, msg: &Message<'a>) -> Result<()> {
        log::trace!("signalling conn sending: {:?}", msg);
        let mut sink = self.sender.lock().await;
        let json = msg.to_json()?;
        sink.send(WsMessage::text(json.clone())).await?;
        Ok(())
    }

    /// Announce existence of a provided [Room]s to others.
    pub async fn announce<'a, R>(&self, rooms: R) -> Result<()>
    where
        R: Iterator<Item = &'a Room>,
    {
        let mut topics = Vec::new();
        let mut announcements = Vec::new();
        for room in rooms {
            topics.push(Cow::from(room.name().deref()));
            announcements.push(Message::Publish {
                topic: Cow::from(room.name().deref()),
                data: SignalEvent::Announce {
                    from: Cow::from(room.peer_id().deref()),
                },
            });
        }
        let mut sink = self.sender.lock().await;
        sink.send(WsMessage::text(Message::Subscribe { topics }.to_json()?))
            .await?;
        for msg in announcements {
            sink.send(WsMessage::text(msg.to_json()?)).await?;
        }

        Ok(())
    }

    /// Subscribes for the [Message]s incoming from the WebSocket server.
    pub fn subscribe(&self) -> SignalingMessages {
        SignalingMessages(self.broadcast.subscribe())
    }

    /// Closes current signaling connection.
    /// Returns `true` if connection was closed due to this method call.
    /// Returns `false` if connection was already closed.
    pub async fn close(&self) -> Result<bool> {
        let msg_handler = self.msg_handler.swap(None);
        if let Some(msg_handler) = msg_handler {
            {
                let mut sink = self.sender.lock().await;
                sink.close().await?;
            }
            let msg_handler = unsafe {
                // since we only use this Arc ref for a purpose of atomic swapping with None
                // when potentially executed from multiple threads
                (Arc::into_raw(msg_handler) as *mut JoinHandle<Result<()>>)
                    .as_mut()
                    .unwrap()
            };
            msg_handler.await??;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[async_trait::async_trait]
impl SignalingConn for WSSignalingConn {
    async fn send(&self, msg: &Message) -> Result<()> {
        self.send(msg).await
    }

    fn subscribe(
        &self,
    ) -> Box<dyn Stream<Item = Result<Arc<Message<'static>>>> + Send + Sync + Unpin> {
        Box::new(self.subscribe())
    }
}

#[derive(Debug)]
pub struct SignalingMessages(tokio::sync::broadcast::Receiver<Arc<Message<'static>>>);

impl Stream for SignalingMessages {
    type Item = Result<Arc<Message<'static>>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use futures_util::Future;

        let mut pinned = unsafe { Pin::new_unchecked(&mut self.0) };
        let fut = pinned.recv();
        pin!(fut);
        match ready!(fut.poll(cx)) {
            Ok(msg) => Poll::Ready(Some(Ok(msg))),
            Err(e) => Poll::Ready(Some(Err(e.into()))),
        }
    }
}

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message<'a> {
    #[serde(rename = "publish")]
    Publish {
        topic: Cow<'a, str>,
        data: SignalEvent<'a>,
    },
    #[serde(rename = "subscribe")]
    Subscribe { topics: Vec<Cow<'a, str>> },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { topics: Vec<Cow<'a, str>> },
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "pong")]
    Pong,
}

impl<'a> Message<'a> {
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalEvent<'a> {
    #[serde(rename = "announce")]
    Announce { from: Cow<'a, str> },
    #[serde(rename = "signal")]
    Signal {
        from: Cow<'a, str>,
        to: Cow<'a, str>,
        signal: wrtc::peer_connection::Signal,
    },
}

impl<'a> SignalEvent<'a> {
    pub fn from(&self) -> &str {
        match self {
            SignalEvent::Announce { from } => from.deref(),
            SignalEvent::Signal { from, .. } => from.deref(),
        }
    }
}

#[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
pub enum PeerEvent {
    Up(PeerId),
    Down(PeerId),
}

#[cfg(test)]
mod test {}
