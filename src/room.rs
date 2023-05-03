use crate::conn::Connection;
use crate::signal::{PeerEvent, SignalEvent, SignalingConn};
use crate::{AwarenessRef, PeerId, Topic};
use crate::{Error, Result};
use bytes::Bytes;
use futures_util::future::try_join_all;
use lib0::encoding::Write;
use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt::Formatter;
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use wrtc::peer_connection::Signal;
use y_sync::awareness::Awareness;
use y_sync::sync::{DefaultProtocol, Message, SyncMessage, MSG_SYNC_UPDATE};
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{uuid_v4, ReadTxn, Transact, UpdateSubscription};

pub(crate) const MSG_SYNC: u8 = 0;
pub(crate) const MSG_QUERY_AWARENESS: u8 = 3;

pub struct Room {
    peer_id: PeerId,
    name: Topic,
    awareness: AwarenessRef,
    signaling_conns: Arc<[Arc<SignalingConn>]>,
    signaling_msg_handlers: Vec<JoinHandle<()>>,
    wrtc_conns: Arc<RwLock<HashMap<Arc<str>, Connection>>>,
    max_conns: usize,
    update_sub: UpdateSubscription,
    awareness_update_sub: y_sync::awareness::UpdateSubscription,
    broadcast_job: JoinHandle<()>,
    peer_events: PeerEvents,
}

impl Room {
    pub fn open<T, I>(name: T, awareness: Awareness, signaling_conns: I) -> Arc<Self>
    where
        Arc<str>: From<T>,
        I: IntoIterator<Item = Arc<SignalingConn>>,
    {
        let signaling_conns: Arc<[Arc<SignalingConn>]> = signaling_conns.into_iter().collect();
        Self::create_internal(Topic::from(name), awareness, signaling_conns, 24)
    }

    pub fn create_internal(
        name: Topic,
        mut awareness: Awareness,
        signaling_conns: Arc<[Arc<SignalingConn>]>,
        max_conns: usize,
    ) -> Arc<Self> {
        let mut peer_id = uuid_v4(&mut rand::thread_rng());
        let doc = awareness.doc();
        let wrtc_conns: Arc<RwLock<HashMap<Arc<str>, Connection>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (broadcast, mut broadcast_rx) = unbounded_channel();
        let (peer_events, _) = tokio::sync::broadcast::channel(1);
        let update_sub = {
            let broadcast = broadcast.clone();
            doc.observe_update_v1(move |_, e| {
                let mut encoder = EncoderV1::new();
                encoder.write_var(MSG_SYNC);
                encoder.write_var(MSG_SYNC_UPDATE);
                encoder.write_buf(&e.update);
                let _ = broadcast.send(encoder.to_vec());
            })
            .unwrap()
        };
        let awareness_update_sub = {
            let broadcast = broadcast.clone();
            awareness.on_update(move |awareness, e| {
                let added = e.added();
                let updated = e.updated();
                let removed = e.removed();
                let mut changed = Vec::with_capacity(added.len() + updated.len() + removed.len());
                changed.extend_from_slice(added);
                changed.extend_from_slice(updated);
                changed.extend_from_slice(removed);

                if let Ok(u) = awareness.update_with_clients(changed) {
                    let _ = broadcast.send(Message::Awareness(u).encode_v1());
                }
            })
        };
        let broadcast_job: JoinHandle<()> = {
            let conns = Arc::downgrade(&wrtc_conns);
            tokio::spawn(async move {
                while let Some(msg) = broadcast_rx.recv().await {
                    if let Some(conns) = conns.upgrade() {
                        let mut conns = conns.write().await;
                        let mut remove = Vec::new();
                        for (peer_id, conn) in conns.iter_mut() {
                            let msg = Bytes::from(msg.clone());
                            if let Err(cause) = conn.send(msg).await {
                                remove.push(peer_id.clone());
                            }
                        }
                        if !remove.is_empty() {
                            // remove failed connections
                            for peer_id in remove {
                                conns.remove(&peer_id);
                            }
                        }
                    } else {
                        break; // room has been closed
                    }
                }
            })
        };
        let awareness = Arc::new(RwLock::new(awareness));
        let signaling_msg_handlers = Vec::with_capacity(signaling_conns.len());
        let mut room = Arc::new(Room {
            peer_id,
            name: name.clone(),
            awareness,
            signaling_conns: signaling_conns.clone(),
            signaling_msg_handlers,
            wrtc_conns,
            max_conns,
            update_sub,
            awareness_update_sub,
            broadcast_job,
            peer_events,
        });
        // the statement below is actually safe, but no... borrow checker has to know better
        let signaling_msg_handlers: &mut Vec<JoinHandle<()>> = unsafe {
            (&room.signaling_msg_handlers as *const Vec<JoinHandle<()>> as *mut Vec<JoinHandle<()>>)
                .as_mut()
                .unwrap()
        };
        for conn in signaling_conns.iter().cloned() {
            let job = tokio::spawn(Room::subscribe(room.clone(), conn));
            signaling_msg_handlers.push(job);
        }
        room
    }

    /// Subscribes for the [Message]s incoming from the WebSocket server.
    async fn subscribe(room: Arc<Room>, conn: Arc<SignalingConn>) {
        let room_ref = Arc::downgrade(&room);
        let mut msgs = conn.subscribe();
        let room_name = room.name.clone();
        let max_conns = room.max_conns;
        let peer_id = room.peer_id.clone();
        let wrtc_conns = Arc::downgrade(&room.wrtc_conns);
        let peers_tx = room.peer_events.clone();
        let awareness = room.awareness.clone();
        let mut msgs = conn.subscribe();
        while let Ok(msg) = msgs.recv().await {
            if let Some(wrtc_conns) = wrtc_conns.upgrade() {
                match msg.deref() {
                    crate::signal::Message::Publish { topic, data }
                        if topic.deref() == room_name.deref() && data.from() != peer_id.deref() =>
                    {
                        match data {
                            SignalEvent::Announce { from } => {
                                let mut conns = wrtc_conns.write().await;
                                let had_already = conns.contains_key(from.deref());
                                if conns.len() < max_conns {
                                    if !had_already {
                                        let remote_peer_id = PeerId::from(from.deref());
                                        log::trace!("'{peer_id}' setting up connection to '{remote_peer_id}' as an initiator");
                                        let result = Connection::new(
                                            awareness.clone(),
                                            Arc::downgrade(&conn),
                                            true,
                                            remote_peer_id.clone(),
                                            room_ref.clone(),
                                        )
                                        .await;
                                        match result {
                                            Ok(conn) => {
                                                conns.insert(remote_peer_id, conn);
                                            }
                                            Err(e) => {
                                                log::trace!("'{peer_id}' failed to init connection to '{remote_peer_id}': {e}");
                                            }
                                        }
                                    }
                                }
                            }
                            SignalEvent::Signal { from, to, signal } => {
                                if to.deref() == peer_id.deref() {
                                    let mut conns = wrtc_conns.write().await;
                                    let remote_peer_id = PeerId::from(from.deref());
                                    let result = match conns.entry(remote_peer_id.clone()) {
                                        Entry::Occupied(mut e) => {
                                            let conn = e.get_mut();
                                            conn.apply(signal.clone()).await
                                        }
                                        Entry::Vacant(e) => {
                                            log::trace!("'{peer_id}' setting up connection to '{remote_peer_id}' as an acceptor");
                                            let result = Connection::new(
                                                awareness.clone(),
                                                Arc::downgrade(&conn),
                                                false,
                                                remote_peer_id.clone(),
                                                room_ref.clone(),
                                            )
                                            .await;
                                            match result {
                                                Ok(conn) => {
                                                    let conn = e.insert(conn);
                                                    conn.apply(signal.clone()).await
                                                }
                                                Err(e) => Err(e),
                                            }
                                        }
                                    };
                                    if let Err(e) = result {
                                        log::trace!("'{peer_id}' failed to process signal from '{remote_peer_id}': {e}");
                                    }
                                }
                            }
                        }
                    }
                    _ => { /* ignore */ }
                }
            }
        }
    }

    pub fn name(&self) -> &Topic {
        &self.name
    }

    pub fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    pub fn awareness(&self) -> &AwarenessRef {
        &self.awareness
    }

    pub fn peer_events(&self) -> &PeerEvents {
        &self.peer_events
    }

    pub(crate) async fn remove_conn(&self, peer_id: &PeerId) {
        let mut conns = self.wrtc_conns.write().await;
        if let Some(_conn) = conns.remove(peer_id) {
            let _ = self.peer_events.send(PeerEvent::Down(peer_id.clone()));
        }
    }

    pub(crate) async fn announce_signaling_info(&self) -> Result<()> {
        let subscribe = crate::signal::Message::Subscribe {
            topics: vec![Cow::from(self.name.deref())],
        };
        let announce = crate::signal::Message::Publish {
            topic: Cow::from(self.name.deref()),
            data: SignalEvent::Announce {
                from: Cow::from(self.peer_id.deref()),
            },
        };

        let connections = {
            let conns = self.wrtc_conns.read().await;
            conns.len()
        };
        let mut at_least_one = false;
        for conn in self.signaling_conns.iter() {
            if let Ok(_) = conn.send(&subscribe).await {
                if connections < self.max_conns {
                    if let Ok(_) = conn.send(&announce).await {
                        at_least_one = true;
                    }
                } else {
                    at_least_one = true;
                }
            }
        }
        if at_least_one {
            Ok(())
        } else {
            Err(Error::from(format!(
                "failed to announce peer '{}' (topic: '{}') on any of the signaling connections",
                self.peer_id, self.name
            )))
        }
    }

    pub async fn is_synced(&self) -> bool {
        let conns = self.wrtc_conns.read().await;
        let mut is_synced = true;
        for conn in conns.values() {
            if !conn.is_synced() {
                is_synced = false;
            }
        }
        is_synced
    }

    pub async fn connect(&self) -> Result<()> {
        // signal through all available signaling connections
        self.announce_signaling_info().await?;
        {
            let awareness = self.awareness.read().await;
            let sv = awareness.doc().transact().state_vector();
            let sync_step1 = Bytes::from(Message::Sync(SyncMessage::SyncStep1(sv)).encode_v1());
            let awareness_query = Bytes::from(Message::AwarenessQuery.encode_v1());
            let conns = self.wrtc_conns.read().await;
            for (remote_peer_id, conn) in conns.iter() {
                conn.send(sync_step1.clone()).await?;
                conn.send(awareness_query.clone()).await?;
            }
        }

        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        {
            // signal through all available signaling connections
            let unsubscribe = crate::signal::Message::Unsubscribe {
                topics: vec![Cow::from(self.name.deref())],
            };
            for conn in self.signaling_conns.iter() {
                let _ = conn.send(&unsubscribe).await;
            }
        }

        {
            let mut awareness = self.awareness.write().await;
            let client_id = awareness.client_id();
            awareness.remove_state(client_id);
        }

        {
            let conns = self.wrtc_conns.write().await;
            for (peer_id, conn) in conns.iter() {
                let _ = conn.close().await;
            }
        }
        Ok(())
    }
}

pub type PeerEvents = tokio::sync::broadcast::Sender<PeerEvent>;

impl std::fmt::Debug for Room {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Room")
            .field("name", &self.name)
            .field("peer_id", &self.peer_id)
            .field("max_conns", &self.max_conns)
            .field("awareness", &self.awareness)
            .field("wrtc_conns", &self.wrtc_conns)
            .field("signaling_conns", &self.signaling_conns)
            .finish()
    }
}
