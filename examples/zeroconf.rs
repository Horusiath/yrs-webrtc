use futures_util::future::try_join;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use y_sync::awareness::Awareness;
use yrs::{GetString, Text, Transact};
use yrs_webrtc::signal::PeerEvent;
use yrs_webrtc::zeroconf::MdnsSignalingConn;
use yrs_webrtc::{Room, SignalingConn};

#[tokio::main]
async fn main() -> yrs_webrtc::Result<()> {
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Trace)
        .try_init();

    let c1: Arc<dyn SignalingConn> = Arc::new(MdnsSignalingConn::new("my-service", 15001)?);
    let r1 = Room::open("test-room", Awareness::default(), [c1]);
    sleep(Duration::from_millis(100)).await;
    let c2: Arc<dyn SignalingConn> = Arc::new(MdnsSignalingConn::new("my-service", 15002)?);
    let r2 = Room::open("test-room", Awareness::default(), [c2]);
    let mut pe1 = r1.peer_events().subscribe();
    let mut pe2 = r2.peer_events().subscribe();

    try_join(r1.connect(), r2.connect()).await?;

    // confirm peers noticed each other
    let e = timeout(Duration::from_secs(1), pe1.recv()).await.unwrap()?;
    assert_eq!(e, PeerEvent::Up(r2.peer_id().clone()));
    let e = timeout(Duration::from_secs(1), pe2.recv()).await.unwrap()?;
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
        .unwrap()
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
