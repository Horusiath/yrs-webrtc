use crate::room::Room;
use crate::signal::SignalingConn;
use crate::Result;
use crate::Topic;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;
use y_sync::awareness::Awareness;
use yrs::Doc;

#[derive(Debug)]
pub struct RoomRegistry {
    signaling_conns: Arc<[Arc<SignalingConn>]>,
    rooms: Mutex<HashMap<Topic, Weak<Room>>>,
    max_connections: usize,
}

impl RoomRegistry {
    pub async fn new(options: Options) -> Result<Self> {
        let mut signaling_conns = Vec::with_capacity(options.signaling.len());
        for url in options.signaling.into_iter() {
            match SignalingConn::connect(url.clone()).await {
                Ok(conn) => signaling_conns.push(Arc::new(conn)),
                Err(err) => {
                    log::error!("failed to open signaling connection to '{url}': {err}");
                }
            }
        }

        if signaling_conns.is_empty() {
            return Err(crate::Error::from("cannot create a new ProviderRegistry with no successfully connected signaling connections"));
        }

        Ok(RoomRegistry {
            signaling_conns: Arc::from(signaling_conns),
            rooms: Mutex::new(HashMap::new()),
            max_connections: options.max_connections,
        })
    }

    pub async fn room<S>(&self, name: S) -> Result<Arc<Room>>
    where
        S: Into<Topic>,
    {
        let options = yrs::Options::default();
        self.room_internal(name.into(), options).await
    }

    pub async fn room_with<S>(&self, name: S, options: yrs::Options) -> Result<Arc<Room>>
    where
        S: Into<Topic>,
    {
        self.room_internal(name.into(), options).await
    }

    async fn room_internal(&self, name: Topic, options: yrs::Options) -> Result<Arc<Room>> {
        let room = {
            let mut rooms = self.rooms.lock().await;
            match rooms.entry(name.clone()) {
                Entry::Occupied(mut e) => {
                    let cell = e.get_mut();
                    if let Some(room) = cell.upgrade() {
                        room
                    } else {
                        let doc = Doc::with_options(options);
                        let awareness = Awareness::new(doc);
                        let room = Room::new(
                            name,
                            awareness,
                            self.signaling_conns.clone(),
                            self.max_connections,
                        );
                        let _ = std::mem::replace(cell, Arc::downgrade(&room));
                        room
                    }
                }
                Entry::Vacant(e) => {
                    let doc = Doc::with_options(options);
                    let awareness = Awareness::new(doc);
                    let room = Room::new(
                        name,
                        awareness,
                        self.signaling_conns.clone(),
                        self.max_connections,
                    );
                    e.insert(Arc::downgrade(&room));
                    room
                }
            }
        };
        room.connect().await?;
        Ok(room)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub signaling: Vec<Arc<str>>,
    pub max_connections: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            signaling: vec![
                Arc::from("wss://signaling.yjs.dev"),
                Arc::from("wss://y-webrtc-signaling-eu.herokuapp.com"),
                Arc::from("wss://y-webrtc-signaling-us.herokuapp.com"),
            ],
            max_connections: 24,
        }
    }
}

#[cfg(test)]
mod test {
    use crate::registry::{Options, RoomRegistry};
    use crate::signal::PeerEvent;
    use crate::Error;
    use log::LevelFilter;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::{sleep, timeout};
    use warp::ws::{WebSocket, Ws};
    use warp::{Filter, Rejection, Reply};
    use yrs_warp::signaling::{signaling_conn, SignalingService};

    const TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn registry_room_connection() -> Result<(), Error> {
        let _ = env_logger::builder()
            .filter_level(LevelFilter::Info)
            .is_test(true)
            .try_init();
        let _ = tokio::spawn(signaling_server());
        sleep(Duration::from_secs(1)).await;

        let options = Options {
            signaling: vec![Arc::from("ws://localhost:8000/signaling")],
            max_connections: 24,
        };

        let registry1 = RoomRegistry::new(options.clone()).await?;
        let registry2 = RoomRegistry::new(options.clone()).await?;
        let r1 = registry1.room("test-room").await?;
        let r2 = registry2.room("test-room").await?;
        let mut pe1 = r1.peer_events().subscribe();
        let mut pe2 = r1.peer_events().subscribe();

        r1.connect().await?;
        r2.connect().await?;

        let e = timeout(TIMEOUT, pe1.recv()).await??;
        assert_eq!(e, PeerEvent::Up(r2.peer_id().clone()));
        let e = timeout(TIMEOUT, pe2.recv()).await??;
        assert_eq!(e, PeerEvent::Up(r1.peer_id().clone()));

        r1.close().await?;
        r2.close().await?;

        Ok(())
    }

    async fn signaling_server() {
        let signaling = SignalingService::new();
        let ws = warp::path("signaling")
            .and(warp::ws())
            .and(warp::any().map(move || signaling.clone()))
            .and_then(ws_handler);

        warp::serve(ws).run(([0, 0, 0, 0], 8000)).await;
    }

    async fn ws_handler(ws: Ws, svc: SignalingService) -> Result<impl Reply, Rejection> {
        Ok(ws.on_upgrade(move |socket| peer(socket, svc)))
    }

    async fn peer(ws: WebSocket, svc: SignalingService) {
        match signaling_conn(ws, svc).await {
            Ok(_) => log::info!("signaling connection stopped"),
            Err(e) => log::error!("signaling connection failed: {}", e),
        }
    }
}
