use crate::room::Room;
use crate::signal::{PeerEvent, SignalEvent, SignalingConn};
use crate::{AwarenessRef, PeerId, Result};
use arc_swap::ArcSwapOption;
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use std::borrow::Cow;
use std::fmt::Formatter;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use wrtc::data_channel::DataChannel;
use wrtc::peer_connection::{PeerConnection, Signal};
use y_sync::sync::{DefaultProtocol, Protocol, SyncMessage};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{ReadTxn, Transact, Update};

#[derive(Debug)]
pub struct Connection {
    remote_peer_id: PeerId,
    peer: Arc<PeerConnection>,
    awareness: AwarenessRef,
    signaling_job: JoinHandle<Result<()>>,
    on_connected: JoinHandle<Result<()>>,
    on_closed: JoinHandle<Result<()>>,
    room: Weak<Room>,
    connected: Arc<ArcSwapOption<ConnectedState>>,
}

impl Connection {
    pub async fn new(
        awareness: AwarenessRef,
        signaling_conn: Weak<SignalingConn>,
        initiator: bool,
        remote_peer_id: PeerId,
        room: Weak<Room>,
    ) -> Result<Self> {
        let room_ref = room.upgrade().unwrap();
        let options = wrtc::peer_connection::Options::with_data_channels(&[&room_ref.name()]);
        let peer_events = room_ref.peer_events().clone();
        let peer = Arc::new(PeerConnection::start(initiator, options).await?);

        let signaling_job = {
            let peer = peer.clone();
            let topic = room_ref.name().clone();
            let peer_id = room_ref.peer_id().clone();
            let signaling_conn = signaling_conn.clone();
            let remote_peer_id = remote_peer_id.clone();
            tokio::spawn(async move {
                while let Some(signal) = peer.signal().await {
                    if let Some(conn) = signaling_conn.upgrade() {
                        let msg = crate::signal::Message::Publish {
                            topic: Cow::from(topic.deref()),
                            data: SignalEvent::Signal {
                                from: Cow::from(peer_id.deref()),
                                to: Cow::from(remote_peer_id.deref()),
                                signal,
                            },
                        };
                        conn.send(&msg).await?;
                    } else {
                        break;
                    }
                }
                Ok(())
            })
        };

        let connected = Arc::new(ArcSwapOption::new(None));
        let on_connected = {
            let peer = peer.clone();
            let awareness = awareness.clone();
            let connected = connected.clone();
            let remote_peer_id = remote_peer_id.clone();
            tokio::spawn(async move {
                peer.connected().await?;
                let channel = peer.data_channels().next().await.unwrap();
                channel.ready().await?;

                let (sink, mut stream) = channel.split();
                let sink = Arc::new(Mutex::new(sink));
                let is_synced = Arc::new(AtomicBool::new(false));

                {
                    let awareness = awareness.read().await;
                    let update = awareness.update()?;
                    let msg = y_sync::sync::Message::Sync(SyncMessage::SyncStep1(
                        awareness.doc().transact().state_vector(),
                    ));
                    let mut sink = sink.lock().await;
                    sink.send(Bytes::from(msg.encode_v1())).await?;
                    let msg = y_sync::sync::Message::Awareness(update);
                    sink.send(Bytes::from(msg.encode_v1())).await?;
                }

                let msg_job: JoinHandle<Result<()>> = {
                    let sink = Arc::downgrade(&sink);
                    let awareness = awareness.clone();
                    let is_synced = is_synced.clone();
                    tokio::spawn(async move {
                        while let Some(msg) = stream.next().await {
                            let msg = y_sync::sync::Message::decode_v1(&msg?.data)?;
                            let reply =
                                handle_msg(&DefaultProtocol, &awareness, msg, &is_synced).await?;
                            if let Some(reply) = reply {
                                if let Some(sink) = sink.upgrade() {
                                    let mut sink = sink.lock().await;
                                    let bytes = Bytes::from(reply.encode_v1());
                                    sink.send(bytes).await?;
                                }
                            }
                        }
                        Ok(())
                    })
                };

                let state = ConnectedState {
                    msg_job,
                    is_synced,
                    channel: sink,
                };
                connected.swap(Some(Arc::new(state)));
                let _ = peer_events.send(PeerEvent::Up(remote_peer_id));
                Ok(())
            })
        };

        let on_closed = {
            let peer = peer.clone();
            let room = room.clone();
            let remote_peer_id = remote_peer_id.clone();
            tokio::spawn(async move {
                let res = peer.closed().await;
                if let Some(room) = room.upgrade() {
                    println!("'{remote_peer_id}' close called");
                    {
                        let mut conns = room.wrtc_conns.write().await;
                        conns.remove(&remote_peer_id);
                    }
                    let _ = room.peer_events().send(PeerEvent::Down(remote_peer_id));
                }
                res.map_err(|e| e.into())
            })
        };

        Ok(Connection {
            remote_peer_id,
            awareness,
            peer,
            room,
            signaling_job,
            on_connected,
            on_closed,
            connected,
        })
    }

    pub fn is_synced(&self) -> bool {
        if let Some(c) = &*self.connected.load() {
            c.is_synced.load(Ordering::Acquire)
        } else {
            false
        }
    }

    pub async fn apply(&self, signal: Signal) -> Result<()> {
        self.peer.apply_signal(signal).await?;
        Ok(())
    }

    pub async fn send(&self, data: Bytes) -> Result<()> {
        if let Some(c) = &*self.connected.load() {
            let mut dc = c.channel.lock().await;
            dc.send(data).await?;
            Ok(())
        } else {
            Err(NotConnected.into())
        }
    }

    pub async fn close(&self) -> Result<()> {
        self.peer.close().await?;
        self.connected.swap(None);
        Ok(())
    }
}

#[derive(Debug)]
struct ConnectedState {
    msg_job: JoinHandle<Result<()>>,
    is_synced: Arc<AtomicBool>,
    channel: Arc<Mutex<SplitSink<DataChannel, Bytes>>>,
}

async fn handle_msg<P: Protocol>(
    protocol: &P,
    a: &AwarenessRef,
    msg: y_sync::sync::Message,
    is_synced: &AtomicBool,
) -> std::result::Result<Option<y_sync::sync::Message>, y_sync::sync::Error> {
    use y_sync::sync::Message;
    match msg {
        Message::Sync(msg) => match msg {
            SyncMessage::SyncStep1(sv) => {
                let awareness = a.read().await;
                protocol.handle_sync_step1(&awareness, sv)
            }
            SyncMessage::SyncStep2(update) => {
                let mut awareness = a.write().await;
                let reply = protocol.handle_sync_step2(&mut awareness, Update::decode_v1(&update)?);
                if reply.is_ok() {
                    is_synced.store(true, Ordering::Release);
                }
                reply
            }
            SyncMessage::Update(update) => {
                let mut awareness = a.write().await;
                protocol.handle_update(&mut awareness, Update::decode_v1(&update)?)
            }
        },
        Message::Auth(reason) => {
            let awareness = a.read().await;
            protocol.handle_auth(&awareness, reason)
        }
        Message::AwarenessQuery => {
            let awareness = a.read().await;
            protocol.handle_awareness_query(&awareness)
        }
        Message::Awareness(update) => {
            let mut awareness = a.write().await;
            protocol.handle_awareness_update(&mut awareness, update)
        }
        Message::Custom(tag, data) => {
            let mut awareness = a.write().await;
            protocol.missing_handle(&mut awareness, tag, data)
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct NotConnected;

impl std::fmt::Display for NotConnected {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "PeerConnection didn't connect to its remote peer")
    }
}

impl std::error::Error for NotConnected {}
