use std::collections::HashMap;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use smithay::output::Output;
use smithay::reexports::calloop;
use zbus::fdo::RequestNameFlags;
use zbus::zvariant::{DeserializeDict, OwnedObjectPath, SerializeDict, Type, Value};
use zbus::{dbus_interface, fdo, InterfaceRef, ObjectServer, SignalContext};

use super::Start;
use crate::utils::output_size;

#[derive(Clone)]
pub struct ScreenCast {
    enabled_outputs: Arc<Mutex<HashMap<String, Output>>>,
    to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    #[allow(clippy::type_complexity)]
    sessions: Arc<Mutex<Vec<(Session, InterfaceRef<Session>)>>>,
}

#[derive(Clone)]
pub struct Session {
    id: usize,
    enabled_outputs: Arc<Mutex<HashMap<String, Output>>>,
    to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    #[allow(clippy::type_complexity)]
    streams: Arc<Mutex<Vec<(Stream, InterfaceRef<Stream>)>>>,
}

#[derive(Debug, Default, Deserialize, Type, Clone, Copy)]
pub enum CursorMode {
    #[default]
    Hidden = 0,
    Embedded = 1,
    Metadata = 2,
}

#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct RecordMonitorProperties {
    #[zvariant(rename = "cursor-mode")]
    cursor_mode: Option<CursorMode>,
    #[zvariant(rename = "is-recording")]
    _is_recording: Option<bool>,
}

#[derive(Clone)]
pub struct Stream {
    output: Output,
    cursor_mode: CursorMode,
    was_started: Arc<AtomicBool>,
    to_niri: calloop::channel::Sender<ScreenCastToNiri>,
}

#[derive(Debug, SerializeDict, Type, Value)]
#[zvariant(signature = "dict")]
struct StreamParameters {
    /// Size of the stream in logical coordinates.
    size: (i32, i32),
}

pub enum ScreenCastToNiri {
    StartCast {
        session_id: usize,
        output: Output,
        cursor_mode: CursorMode,
        signal_ctx: SignalContext<'static>,
    },
    StopCast {
        session_id: usize,
    },
    Redraw(Output),
}

#[dbus_interface(name = "org.gnome.Mutter.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        properties: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<OwnedObjectPath> {
        if properties.contains_key("remote-desktop-session-id") {
            return Err(fdo::Error::Failed(
                "there are no remote desktop sessions".to_owned(),
            ));
        }

        static NUMBER: AtomicUsize = AtomicUsize::new(0);
        let session_id = NUMBER.fetch_add(1, Ordering::SeqCst);
        let path = format!("/org/gnome/Mutter/ScreenCast/Session/u{}", session_id);
        let path = OwnedObjectPath::try_from(path).unwrap();

        let session = Session::new(
            session_id,
            self.enabled_outputs.clone(),
            self.to_niri.clone(),
        );
        match server.at(&path, session.clone()).await {
            Ok(true) => {
                let iface = server.interface(&path).await.unwrap();
                self.sessions.lock().unwrap().push((session, iface));
            }
            Ok(false) => return Err(fdo::Error::Failed("session path already exists".to_owned())),
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating session object: {err:?}"
                )))
            }
        }

        Ok(path)
    }

    #[dbus_interface(property)]
    async fn version(&self) -> i32 {
        4
    }
}

#[dbus_interface(name = "org.gnome.Mutter.ScreenCast.Session")]
impl Session {
    async fn start(&self) {
        debug!("start");

        for (stream, iface) in &*self.streams.lock().unwrap() {
            stream.start(self.id, iface.signal_context().clone());
        }
    }

    pub async fn stop(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_context)] ctxt: SignalContext<'_>,
    ) {
        debug!("stop");

        Session::closed(&ctxt).await.unwrap();

        if let Err(err) = self.to_niri.send(ScreenCastToNiri::StopCast {
            session_id: self.id,
        }) {
            warn!("error sending StopCast to niri: {err:?}");
        }

        let streams = mem::take(&mut *self.streams.lock().unwrap());
        for (_, iface) in streams.iter() {
            server
                .remove::<Stream, _>(iface.signal_context().path())
                .await
                .unwrap();
        }

        server.remove::<Session, _>(ctxt.path()).await.unwrap();
    }

    async fn record_monitor(
        &mut self,
        #[zbus(object_server)] server: &ObjectServer,
        connector: &str,
        properties: RecordMonitorProperties,
    ) -> fdo::Result<OwnedObjectPath> {
        debug!(connector, ?properties, "record_monitor");

        let Some(output) = self.enabled_outputs.lock().unwrap().get(connector).cloned() else {
            return Err(fdo::Error::Failed("no such monitor".to_owned()));
        };

        static NUMBER: AtomicUsize = AtomicUsize::new(0);
        let path = format!(
            "/org/gnome/Mutter/ScreenCast/Stream/u{}",
            NUMBER.fetch_add(1, Ordering::SeqCst)
        );
        let path = OwnedObjectPath::try_from(path).unwrap();

        let cursor_mode = properties.cursor_mode.unwrap_or_default();

        let stream = Stream::new(output, cursor_mode, self.to_niri.clone());
        match server.at(&path, stream.clone()).await {
            Ok(true) => {
                let iface = server.interface(&path).await.unwrap();
                self.streams.lock().unwrap().push((stream, iface));
            }
            Ok(false) => return Err(fdo::Error::Failed("stream path already exists".to_owned())),
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating stream object: {err:?}"
                )))
            }
        }

        Ok(path)
    }

    #[dbus_interface(signal)]
    async fn closed(ctxt: &SignalContext<'_>) -> zbus::Result<()>;
}

#[dbus_interface(name = "org.gnome.Mutter.ScreenCast.Stream")]
impl Stream {
    #[dbus_interface(signal)]
    pub async fn pipe_wire_stream_added(ctxt: &SignalContext<'_>, node_id: u32)
        -> zbus::Result<()>;

    #[dbus_interface(property)]
    async fn parameters(&self) -> StreamParameters {
        let size = output_size(&self.output).into();
        StreamParameters { size }
    }
}

impl ScreenCast {
    pub fn new(
        enabled_outputs: Arc<Mutex<HashMap<String, Output>>>,
        to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    ) -> Self {
        Self {
            enabled_outputs,
            to_niri,
            sessions: Arc::new(Mutex::new(vec![])),
        }
    }
}

impl Start for ScreenCast {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server()
            .at("/org/gnome/Mutter/ScreenCast", self)?;
        conn.request_name_with_flags("org.gnome.Mutter.ScreenCast", flags)?;

        Ok(conn)
    }
}

impl Session {
    pub fn new(
        id: usize,
        enabled_outputs: Arc<Mutex<HashMap<String, Output>>>,
        to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    ) -> Self {
        Self {
            id,
            enabled_outputs,
            streams: Arc::new(Mutex::new(vec![])),
            to_niri,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.to_niri.send(ScreenCastToNiri::StopCast {
            session_id: self.id,
        });
    }
}

impl Stream {
    pub fn new(
        output: Output,
        cursor_mode: CursorMode,
        to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    ) -> Self {
        Self {
            output,
            cursor_mode,
            was_started: Arc::new(AtomicBool::new(false)),
            to_niri,
        }
    }

    fn start(&self, session_id: usize, ctxt: SignalContext<'static>) {
        if self.was_started.load(Ordering::SeqCst) {
            return;
        }

        let msg = ScreenCastToNiri::StartCast {
            session_id,
            output: self.output.clone(),
            cursor_mode: self.cursor_mode,
            signal_ctx: ctxt,
        };

        if let Err(err) = self.to_niri.send(msg) {
            warn!("error sending StartCast to niri: {err:?}");
        }
    }
}
