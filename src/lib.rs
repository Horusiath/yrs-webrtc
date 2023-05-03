pub mod conn;
pub mod room;
pub mod signal;

use std::sync::Arc;
use tokio::sync::RwLock;
use y_sync::awareness::Awareness;

pub type AwarenessRef = Arc<RwLock<Awareness>>;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
pub type Topic = Arc<str>;
pub type PeerId = Arc<str>;
pub type Room = crate::room::Room;
pub type SignalingConn = crate::signal::SignalingConn;

#[cfg(test)]
mod test {
    use crate::signal::PeerEvent;
    use crate::{Error, Room, SignalingConn};
    use futures_util::future::try_join;
    use log::LevelFilter;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::{sleep, timeout};
    use warp::ws::{WebSocket, Ws};
    use warp::{Filter, Rejection, Reply};
    use y_sync::awareness::Awareness;
    use yrs::{GetString, Text, Transact};
    use yrs_warp::signaling::{signaling_conn, SignalingService};

    #[tokio::test]
    async fn basic_message_exchange() -> Result<(), Error> {
        //let _ = env_logger::builder()
        //    .filter_level(LevelFilter::Info)
        //    .is_test(true)
        //    .try_init();
        let _ = tokio::spawn(signaling_server());
        sleep(Duration::from_secs(1)).await;

        let c1 = Arc::new(SignalingConn::connect("ws://localhost:8000/signaling").await?);
        let r1 = Room::open("test-room", Awareness::default(), [c1]);
        let c2 = Arc::new(SignalingConn::connect("ws://localhost:8000/signaling").await?);
        let r2 = Room::open("test-room", Awareness::default(), [c2]);
        let mut pe1 = r1.peer_events().subscribe();
        let mut pe2 = r2.peer_events().subscribe();

        try_join(r1.connect(), r2.connect()).await?;

        // confirm peers noticed each other
        let e = pe1.recv().await?;
        assert_eq!(e, PeerEvent::Up(r2.peer_id().clone()));
        let e = pe2.recv().await?;
        assert_eq!(e, PeerEvent::Up(r1.peer_id().clone()));

        // subscribe to document updates on R1
        let (tx, mut rx) = tokio::sync::watch::channel(());
        let _sub = {
            let awareness = r1.awareness().write().await;
            let doc = awareness.doc();
            let _ = doc.get_or_insert_text("test");
            doc.observe_update_v1(move |_, _| {
                let _ = tx.send(());
            })
            //.unwrap()
        };

        // make change on R2
        {
            let awareness = r2.awareness().write().await;
            let doc = awareness.doc();
            let text = doc.get_or_insert_text("test");
            text.push(&mut doc.transact_mut(), "abc");
        }

        rx.changed().await?; // wait for change to be propagated to R1

        // check change on R1
        {
            let awareness = r1.awareness().read().await;
            let doc = awareness.doc();
            let text = doc.get_or_insert_text("test");
            let str = text.get_string(&doc.transact());
            assert_eq!(&str, "abc");
        }

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
