use crate::signal::{Message, SignalEvent};
use crate::SignalingConn;
use async_stream::try_stream;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::{channel, Sender};
use tokio::sync::Mutex;
use tokio::task::{spawn_blocking, JoinHandle};
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use wrtc::peer_connection::Signal;
use zeroconf::event_loop::TEventLoop;
use zeroconf::prelude::{TMdnsBrowser, TMdnsService, TTxtRecord};
use zeroconf::{EventLoop, MdnsBrowser, MdnsService, ServiceDiscovery, ServiceType, TxtRecord};

#[repr(transparent)]
struct SendableEventLoop<'a>(EventLoop<'a>);

unsafe impl<'a> Send for SendableEventLoop<'a> {}

impl<'a> Deref for SendableEventLoop<'a> {
    type Target = EventLoop<'a>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> DerefMut for SendableEventLoop<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Debug, Clone)]
pub struct MdnsSignalingConn {
    inner: Arc<Mutex<Inner>>,
}

impl MdnsSignalingConn {
    pub fn new(service_name: &str, port: u16) -> crate::Result<Self> {
        let service_type = ServiceType::new(service_name, "udp")?;
        let service = MdnsService::new(service_type.clone(), port);
        let mut browser = MdnsBrowser::new(service_type);
        let (discovered_services, _) = channel(32);
        let ds = discovered_services.clone();
        browser.set_service_discovered_callback(Box::new(move |res, context| {
            if let Ok(d) = res {
                log::debug!("discovered a new agent: {}", d.name());
                match handle_discovered(d) {
                    Ok(messages) => {
                        for msg in messages {
                            if let Err(e) = ds.send(Arc::new(msg)) {
                                log::warn!(
                                    "failed to announce message due to closed channel: {:?}",
                                    e.0.as_ref()
                                );
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("failed to parse incoming service discovery: {}", e);
                    }
                }
            }
        }));
        let event_loop =
            SendableEventLoop(unsafe { std::mem::transmute(browser.browse_services()?) });
        let browser_task = spawn_blocking(move || loop {
            if let Err(e) = event_loop.poll(Duration::from_secs(0)) {
                log::error!("browser event loop failed with: {}", e);
                break;
            }
        });
        let inner = Arc::new(Mutex::new(Inner {
            service,
            browser,
            browser_task,
            discovered_services,
            service_task: None,
            manifest: Manifest::default(),
            negotiating: HashSet::new(),
        }));
        Ok(MdnsSignalingConn { inner })
    }
}

fn handle_discovered(sd: ServiceDiscovery) -> crate::Result<Vec<Message<'static>>> {
    let mut messages = Vec::new();
    if let Some(record) = sd.txt() {
        let from = Cow::from(sd.name().clone());
        let topics: Vec<String> = if let Some(topics) = record.get("topics") {
            serde_json::from_str(&topics)?
        } else {
            Vec::new()
        };
        if let Some(sdp) = parse_sdp(record) {
            let to = Cow::from(sdp.recipient);
            let sdp = match sdp.sdp_type.as_str() {
                "offer" => RTCSessionDescription::offer(sdp.sdp)?,
                "answer" => RTCSessionDescription::answer(sdp.sdp)?,
                "pranswer" => RTCSessionDescription::pranswer(sdp.sdp)?,
                other => panic!("unrecognized sdp type: {}", other),
            };
            for topic in topics {
                let msg = Message::Publish {
                    topic: topic.into(),
                    data: SignalEvent::Signal {
                        from: from.clone(),
                        to: to.clone(),
                        signal: Signal::Sdp(sdp.clone()),
                    },
                };
                log::trace!("received from {}: {:?}", from, msg);
                messages.push(msg);
            }
        } else {
            for topic in topics {
                let msg = Message::Publish {
                    topic: topic.into(),
                    data: SignalEvent::Announce { from: from.clone() },
                };
                log::trace!("received from {}: {:?}", from, msg);
                messages.push(msg);
            }
        }
    }
    Ok(messages)
}

fn parse_sdp(record: &TxtRecord) -> Option<ManifestSdp> {
    let recipient = record.get("sdp_recipient")?;
    let sdp_type = record.get("sdp_type")?;
    let sdp = record.get("sdp")?;
    Some(ManifestSdp {
        recipient,
        sdp,
        sdp_type,
    })
}

#[async_trait::async_trait]
impl SignalingConn for MdnsSignalingConn {
    async fn send<'a>(&self, msg: &Message<'a>) -> crate::Result<()> {
        let mut guard = self.inner.lock().await;
        let mut manifest = &mut guard.manifest;
        let mut manifest_changed = false;
        match msg {
            Message::Publish { topic, data } => {
                manifest_changed |= manifest.topics.insert(topic.to_string());
                match data {
                    SignalEvent::Announce { from } => {
                        manifest.name = Some(from.to_string());
                        manifest_changed = true;
                    }
                    SignalEvent::Signal { from, to, signal } => {
                        if Some(from.as_ref()) == manifest.name.as_deref() {
                            match signal {
                                Signal::Renegotiate(true) => {
                                    *manifest = Manifest {
                                        name: Some(from.to_string()),
                                        topics: Default::default(),
                                        sdp: None,
                                    };
                                    manifest_changed |= true;
                                }
                                Signal::Sdp(sdp) => {
                                    manifest.sdp = Some(ManifestSdp {
                                        recipient: to.to_string(),
                                        sdp: sdp.sdp.clone(),
                                        sdp_type: sdp.sdp_type.to_string(),
                                    });
                                    manifest_changed |= true;
                                }
                                _ => { /* ignore for now */ }
                            }
                        } else {
                            log::debug!("ignoring incoming signal from {}", from);
                        }
                    }
                }
            }
            Message::Subscribe { topics } => {
                for topic in topics.iter() {
                    manifest_changed |= manifest.topics.insert(topic.to_string());
                }
            }
            Message::Unsubscribe { topics } => {
                for topic in topics.iter() {
                    manifest_changed |= manifest.topics.remove(topic.as_ref());
                }
            }
            Message::Ping => { /* do nothing */ }
            Message::Pong => { /* do nothing */ }
        }
        if manifest_changed {
            guard.publish_manifest()?;
        }
        Ok(())
    }

    fn subscribe(
        &self,
    ) -> Pin<Box<dyn Stream<Item = crate::Result<Arc<Message<'static>>>> + Send + Sync>> {
        let inner = Arc::downgrade(&self.inner);
        Box::pin(try_stream! {
            if let Some(inner) = inner.upgrade() {
                let mut receiver = {
                    let guard = inner.lock().await;
                    guard.discovered_services.subscribe()
                };
                while let Ok(msg) = receiver.recv().await {
                    yield msg;
                }
            }
        })
    }
}

#[derive(Debug)]
pub struct Inner {
    service: MdnsService,
    service_task: Option<JoinHandle<()>>,
    browser: MdnsBrowser,
    browser_task: JoinHandle<()>,
    manifest: Manifest,
    discovered_services: Sender<Arc<Message<'static>>>,
    negotiating: HashSet<String>,
}

unsafe impl Send for Inner {}

impl Inner {
    fn publish_manifest(&mut self) -> crate::Result<()> {
        log::trace!("publishing new service manifest: {:?}", self.manifest);
        if let Some(name) = &self.manifest.name {
            if let Some(el) = self.service_task.take() {
                log::trace!("dropping previous service task");
                drop(el);
            }
            self.service.set_name(name);
        } else {
            log::trace!("skipping manifest publish: service name not known yet");
            return Ok(());
        }
        let mut record = TxtRecord::new();
        if !self.manifest.topics.is_empty() {
            record.insert("topics", &serde_json::to_string(&self.manifest.topics)?)?;
        }
        if let Some(sdp) = self.manifest.sdp.take() {
            record.insert("sdp_recipient", &sdp.recipient)?;
            record.insert("sdp_type", &sdp.sdp_type)?;
            record.insert("sdp", &sdp.sdp)?;
        }
        log::debug!(
            "publishing bonjour record for {:?}: {:?}",
            self.service.name(),
            record
        );
        self.service.set_txt_record(record);
        let event_loop =
            SendableEventLoop::from(unsafe { std::mem::transmute(self.service.register()?) });
        let service_task = spawn_blocking(move || loop {
            if let Err(e) = event_loop.poll(Duration::from_secs(0)) {
                log::error!("service event loop failed with: {}", e);
                break;
            }
        });
        self.service_task = Some(service_task);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Manifest {
    name: Option<String>,
    topics: HashSet<String>,
    sdp: Option<ManifestSdp>,
}

#[derive(Debug)]
struct ManifestSdp {
    recipient: String,
    sdp: String,
    sdp_type: String,
}

#[cfg(test)]
mod test {
    use crate::signal::PeerEvent;
    use crate::zeroconf::MdnsSignalingConn;
    use crate::{Error, Room, SignalingConn};
    use futures_util::future::try_join;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::timeout;
    use y_sync::awareness::Awareness;
    use yrs::{GetString, Text, Transact};

    #[tokio::test]
    async fn zeroconf_signaling() -> Result<(), Error> {
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Trace)
            .is_test(true)
            .try_init();
        let c1: Arc<dyn SignalingConn> = Arc::new(MdnsSignalingConn::new("my-service", 15001)?);
        let r1 = Room::open("test-room", Awareness::default(), [c1]);
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
}
