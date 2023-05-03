use futures_util::future::try_join;
use log::LevelFilter;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio::try_join;
use warp::ws::{WebSocket, Ws};
use warp::{Filter, Rejection, Reply};
use y_sync::awareness::Awareness;
use yrs::{GetString, Text, Transact};
use yrs_warp::signaling::{signaling_conn, SignalingService};
use yrs_webrtc::signal::PeerEvent;
use yrs_webrtc::{Error, Room, SignalingConn};

const STATIC_FILES_DIR: &str = "examples/webrtc-signaling-server/frontend/dist";

#[tokio::main]
async fn main() -> Result<(), Error> {
    let _ = env_logger::builder()
        .filter_level(LevelFilter::Trace)
        .is_test(true)
        .try_init();

    let x = tokio::spawn(signaling_server());

    let _ = tokio::spawn(signaling_server());
    sleep(Duration::from_secs(1)).await;

    let c1 = Arc::new(SignalingConn::connect("ws://localhost:8000/signaling").await?);
    let r1 = Room::open("test-room", Awareness::default(), [c1]);
    let c2 = Arc::new(SignalingConn::connect("ws://localhost:8000/signaling").await?);
    let r2 = Room::open("test-room", Awareness::default(), [c2]);
    let mut pe1 = r1.peer_events().subscribe();
    let mut pe2 = r2.peer_events().subscribe();

    try_join(r1.connect(), r2.connect()).await?;

    let e = pe1.recv().await?;
    assert_eq!(e, PeerEvent::Up(r2.peer_id().clone()));
    let e = pe2.recv().await?;
    assert_eq!(e, PeerEvent::Up(r1.peer_id().clone()));

    let (s, mut rx) = tokio::sync::watch::channel(());
    let _sub = {
        let a = r1.awareness().write().await;
        a.doc().observe_update_v1(move |txn, u| {
            let _ = s.send(());
        })
    };

    {
        let a = r2.awareness().write().await;
        let doc = a.doc();
        let txt = doc.get_or_insert_text("text");
        txt.push(&mut doc.transact_mut(), "abc");
    }

    rx.changed().await?;

    {
        let a = r1.awareness().write().await;
        let doc = a.doc();
        let txt = doc.get_or_insert_text("text");
        let str = txt.get_string(&doc.transact());
        assert_eq!(str, "abc");
    }

    try_join(r1.close(), r2.close()).await?;

    Ok(())
}

async fn signaling_server() {
    let signaling = SignalingService::new();

    let static_files = warp::get().and(warp::fs::dir(STATIC_FILES_DIR));

    let ws = warp::path("signaling")
        .and(warp::ws())
        .and(warp::any().map(move || signaling.clone()))
        .and_then(ws_handler);

    let routes = ws.or(static_files);

    warp::serve(routes).run(([0, 0, 0, 0], 8000)).await;
}

async fn ws_handler(ws: Ws, svc: SignalingService) -> Result<impl Reply, Rejection> {
    Ok(ws.on_upgrade(move |socket| peer(socket, svc)))
}

async fn peer(ws: WebSocket, svc: SignalingService) {
    match signaling_conn(ws, svc).await {
        Ok(_) => println!("signaling connection stopped"),
        Err(e) => eprintln!("signaling connection failed: {}", e),
    }
}
