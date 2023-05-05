# yrs-webrtc

This is a [Yrs](https://docs.rs/yrs/latest/yrs/) integrated network provider which makes use of WebRTC protocol to propagate changes.
It's compatible with [y-webrtc](https://github.com/yjs/y-webrtc) JavaScript client and makes use of the same signaling server protocol (there are  [Node.js](https://github.com/yjs/y-webrtc/blob/master/bin/server.js) and [Rust](https://github.com/y-crdt/yrs-warp/blob/master/src/signaling.rs) implementations of that protocol for you to use).

## Example

```rust
use yrs_webrtc::signal::PeerEvent;
use yrs_webrtc::{Error, Room, SignalingConn};

// start a WebRTC peer 
async fn peer(topic: &str) -> Result<(), Error> {
    // create a signaling client
    let conn = Arc::new(SignalingConn::connect("ws://localhost:8000/signaling").await?);
    // create a new peer collaborating on a given topic
    let room = Room::open(topic, Awareness::default(), [conn]);
    let mut peer_events = room.peer_events().subscribe();

    // wait for room to initialize connection to signaling server
    room.connect().await?;

    // listen for events about connecting/disconnecting peers
    let _ = tokio::spawn(async move {
        while let Ok(e) = peer_events.recv().await {
            println!("received peer event: {e:?}");
        }
    });

    // watch for incoming updates
    let _ = {
        let awareness = room.awareness().write().await;
        awareness.doc().observe_update_v1(move |txn, u| {
            let u = Update::decode_v1(&u.update).unwrap();
            println!("received update: {u:?}");
        })
    };

    // keep room alive and close it when you're done
    room.close().await?;
}

// create y-webrtc/yrs-webrtc compatible signaling server
use yrs_warp::signaling::{signaling_conn, SignalingService};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let signaling = SignalingService::new();

    let ws = warp::path("signaling")
        .and(warp::ws())
        .and(warp::any().map(move || signaling.clone()))
        .and_then(ws_handler);

    warp::serve(ws).run(([0, 0, 0, 0], 8000)).await;
    Ok(())
}

async fn ws_handler(ws: Ws, svc: SignalingService) -> Result<impl Reply, Rejection> {
    Ok(ws.on_upgrade(move |socket| ws_conn(socket, svc)))
}

async fn ws_conn(ws: WebSocket, svc: SignalingService) {
    match signaling_conn(ws, svc).await {
        Ok(_) => println!("signaling connection stopped"),
        Err(e) => eprintln!("signaling connection failed: {}", e),
    }
}
```