//! Sound mixing and device infrastructure.
use portaudio as pa;
use uuid::Uuid;
use std::ops::DerefMut;
use std::rc::Rc;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::collections::BTreeMap;
use bounded_spsc_queue;
use bounded_spsc_queue::{Producer, Consumer};
pub const FRAMES_PER_CALLBACK: usize = 500;

/// Fills a given buffer with silence.
pub fn fill_with_silence(buf: &mut [f32], zero: bool) {
    for smpl in buf.iter_mut() {
        if zero {
            *smpl = 0.0;
        }
    }
}
/// Describes objects that can accept audio data from `Source`s.
pub trait Sink {
    /// Wires a given client to this sink.
    ///
    /// If this sink can only hold one client and it is full already,
    /// returns that client.
    fn wire(&mut self, cli: Box<Source>) -> Option<Box<Source>>;
    /// Retrieves the client with a given `uuid` from this sink.
    fn unwire(&mut self, uuid: Uuid) -> Option<Box<Source>>;
    /// Called on sink destruction. Returns all sources this sink owns.
    fn drop(&mut self) -> Vec<Box<Source>>;
    /// Get this object's Universally Unique Identifier (UUID).
    fn uuid(&self) -> Uuid;
}

/// Describes objects that can provide a stream of audio data.
pub trait Source {
    /// Get more audio data from this object.
    ///
    /// As this is often called in a low-latency audio thread, it must try its best
    /// to not block and be efficient.
    fn callback(&mut self, buffer: &mut [f32], frames: usize, zero: bool);
    /// Get this object's sample rate.
    fn sample_rate(&self) -> u64;
    /// Get this object's Universally Unique Identifier (UUID).
    fn uuid(&self) -> Uuid;
}
/// Details return values from a call to `wire()`.
#[derive(Debug)]
pub enum WireResult {
    /// Source could not be found (standalone, or wired to a known sink).
    SourceNotAvailable,
    /// Sink could not be found (not in `sinks` vec?).
    SinkNotAvailable,
    /// Successful, but had to displace a source of given UUID.
    ///
    /// (The source is placed into the `sources` of the `Magister`).
    DisplacedUuid(Uuid),
    /// Successful, nothing cool happened.
    Uneventful
}

/// Master of all things mixer-y.
///
/// Contains various `BTreeMap`s of everything related to mixing.
///
/// *magister, magistri (2nd declension masculine): master, teacher*
pub struct Magister {
    /// Map of sink UUIDs to sinks.
    sinks: BTreeMap<Uuid, Box<Sink>>,
    /// Map of source UUIDs to sources.
    sources: BTreeMap<Uuid, Box<Source>>,
    /// Vector of intermediate channel UUIDs in (source, sink) format.
    pub ichans: Vec<(Uuid, Uuid)>
}

impl Magister {
    pub fn new() -> Self {
        let mut ms = Magister {
            sinks: BTreeMap::new(),
            sources: BTreeMap::new(),
            ichans: Vec::new()
        };
        for _ in 0..16 {
            ms.add_ich()
        }
        ms
    }
    pub fn add_ich(&mut self) {
        let (qch, qchx) = QChannel::new(44_100);
        self.ichans.push((qch.uuid(), qchx.uuid()));
        self.add_source(Box::new(qch));
        self.add_sink(Box::new(qchx));
    }
    pub fn add_source(&mut self, source: Box<Source>) {
        if let Some(_) = self.sources.insert(source.uuid(), source) {
            panic!("UUID collision")
        }
    }
    pub fn add_sink(&mut self, sink: Box<Sink>) {
        if let Some(_) = self.sinks.insert(sink.uuid(), sink) {
            panic!("UUID collision")
        }
    }
    pub fn locate_source(&mut self, from: Uuid) -> Option<Box<Source>> {
        let mut source: Option<Box<Source>> = self.sources.remove(&from);
        if source.is_none() {
            for (_, val) in self.sinks.iter_mut() {
                if let Some(src) = val.unwire(from) {
                    source = Some(src);
                    break;
                }
            }
        }
        source
    }
    pub fn locate_sink(&mut self, from: Uuid) -> Option<Box<Sink>> {
        if let Some(mut snk) = self.sinks.remove(&from) {
            for src in snk.drop() {
                self.add_source(src);
            }
            Some(snk)
        }
        else {
            None
        }
    }
    pub fn wire(&mut self, from: Uuid, to: Uuid) -> Result<WireResult, WireResult> {
        let source = self.locate_source(from);
        if source.is_none() {
            return Err(WireResult::SourceNotAvailable)
        }
        let result: Option<Box<Source>>;
        {
            let sink: Option<&mut Box<Sink>> = self.sinks.get_mut(&to);
            if sink.is_none() {
                return Err(WireResult::SinkNotAvailable)
            }
            result = sink.unwrap().wire(source.unwrap());
        }
        if let Some(displaced) = result {
            let uuid = displaced.uuid();
            self.add_source(displaced);
            Ok(WireResult::DisplacedUuid(uuid))
        }
        else {
            Ok(WireResult::Uneventful)
        }
    }

}
/// Sink that routes audio to a PortAudio stream output.
pub struct DeviceSink {
    pub stream: Rc<RefCell<pa::stream::Stream<pa::stream::NonBlocking, pa::stream::Output<f32>>>>,
    txrx: Arc<Mutex<(Producer<(usize, Option<Box<Source>>)>, Consumer<Option<Box<Source>>>)>>,
    last_uuid_wired: Uuid,
    id: usize,
    uuid: Uuid
}
impl Sink for DeviceSink {
    fn wire(&mut self, cli: Box<Source>) -> Option<Box<Source>> {
        let mut lck = self.txrx.lock().unwrap();
        let &mut (ref mut tx, ref mut rx) = lck.deref_mut();
        self.last_uuid_wired = cli.uuid();
        tx.push((self.id, Some(cli)));
        rx.pop()
    }
    fn unwire(&mut self, uuid: Uuid) -> Option<Box<Source>> {
        if self.last_uuid_wired == uuid {
            let mut lck = self.txrx.lock().unwrap();
            let &mut (ref mut tx, ref mut rx) = lck.deref_mut();
            self.last_uuid_wired = Uuid::new_v4();
            tx.push((self.id, None));
            rx.pop()
        }
        else {
            None
        }
    }
    fn drop(&mut self) -> Vec<Box<Source>> {
        let mut lck = self.txrx.lock().unwrap();
        let &mut (ref mut tx, ref mut rx) = lck.deref_mut();
        tx.push((self.id, None));
        rx.pop().map(|x| vec![x]).unwrap_or(vec![])
    }
    fn uuid(&self) -> Uuid {
        self.uuid.clone()
    }
}
impl DeviceSink {
    /// Creates a new DeviceSink from a PortAudio context, device index.
    ///
    /// Will optionally use given vector of UUIDs as the sink UUIDs.
    pub fn from_device_chans(pa: &mut pa::PortAudio, dev: pa::DeviceIndex, uu: Vec<Uuid>) -> Result<Vec<Self>, pa::error::Error> {
        let dev_info = try!(pa.device_info(dev));
        let params: pa::StreamParameters<f32> = pa::StreamParameters::new(dev, dev_info.max_output_channels, false, dev_info.default_low_output_latency);
        try!(pa.is_output_format_supported(params, 44_100.0_f64));
        let settings = pa::stream::OutputSettings::new(params, 44_100.0_f64, FRAMES_PER_CALLBACK as u32);

        let mut chans = Vec::new();
        let (dc0, cd0) = bounded_spsc_queue::make(dev_info.max_output_channels as usize + 5);
        let (cd1, dc1) = bounded_spsc_queue::make(dev_info.max_output_channels as usize + 5);
        let ds_to_cb: Arc<Mutex<(Producer<(usize, Option<Box<Source>>)>, Consumer<Option<Box<Source>>>)>> = Arc::new(Mutex::new((dc0, dc1)));
        let cb_to_ds: (Consumer<(usize, Option<Box<Source>>)>, Producer<Option<Box<Source>>>) = (cd0, cd1);

        let mut bufs: Vec<Vec<f32>> = Vec::new();
        for _ in 0..dev_info.max_output_channels {
            chans.push(None);
            let mut buf: Vec<f32> = Vec::new();
            for _ in 0..FRAMES_PER_CALLBACK {
                buf.push(0.0);
            }
            bufs.push(buf);
        }
        let callback = move |pa::stream::OutputCallbackArgs { buffer, frames, .. }| {
            assert!(frames <= FRAMES_PER_CALLBACK as usize, "PA demanded more frames/cb than we asked for");
            if let Some((i,c)) = cb_to_ds.0.try_pop() {
                cb_to_ds.1.push(chans[i].take());
                chans[i] = c;
            }

            /* FIXME: rust-portaudio need to add support for interleaved buffers. Until they do this,
            this unsafe part has to stay. */
            unsafe {
                let buffer: *mut *mut f32 = ::std::mem::transmute(buffer.get_unchecked_mut(0));
                let buffer: &mut [*mut f32] = ::std::slice::from_raw_parts_mut(buffer, chans.len());
                for (i, ch) in chans.iter_mut().enumerate() {
                    if ch.is_some() {
                        ch.as_mut().unwrap().callback(::std::slice::from_raw_parts_mut(buffer[i], frames), frames, false);
                    }
                }
            }
            pa::Continue
        };
        let stream = Rc::new(RefCell::new(try!(pa.open_non_blocking_stream(settings, callback))));
        {
            stream.borrow_mut().start().unwrap();
        }
        let mut rets: Vec<Self> = Vec::new();
        for i in 0..dev_info.max_output_channels as usize {
            rets.push(DeviceSink {
                stream: stream.clone(),
                txrx: ds_to_cb.clone(),
                uuid: uu.get(i).map(|x| *x).unwrap_or(Uuid::new_v4()),
                last_uuid_wired: Uuid::new_v4(),
                id: i
            })
        }
        Ok(rets)
    }
}
enum QCXRequest {
    PushClient(Box<Source>),
    GetClient(Uuid)
}
/// An intermediate channel that streams are automatically wired to - sink side.
pub struct QChannelX {
    tx: Producer<QCXRequest>,
    rx: Consumer<Option<Box<Source>>>,
    sent_uuids: Vec<Uuid>,
    uuid: Uuid
}
impl Sink for QChannelX {
    fn uuid(&self) -> Uuid {
        self.uuid.clone()
    }
    fn wire(&mut self, cli: Box<Source>) -> Option<Box<Source>> {
        self.sent_uuids.push(cli.uuid());
        self.tx.push(QCXRequest::PushClient(cli));
        None
    }
    fn unwire(&mut self, uuid: Uuid) -> Option<Box<Source>> {
        if let Some(pos) = self.sent_uuids.iter().position(|&uu| uu == uuid) {
            self.sent_uuids.remove(pos);
            self.tx.push(QCXRequest::GetClient(uuid));
            self.rx.pop()
        }
        else {
            None
        }
    }
    fn drop(&mut self) -> Vec<Box<Source>> {
        let mut ret = vec![];
        for uu in self.sent_uuids.iter_mut() {
            self.tx.push(QCXRequest::GetClient(*uu));
            if let Some(cli) = self.rx.pop() {
                ret.push(cli);
            }
        }
        ret
    }
}
/// An intermediate channel that streams are automatically wired to - source side.
pub struct QChannel {
    clients: Vec<Box<Source>>,
    rx: Consumer<QCXRequest>,
    tx: Producer<Option<Box<Source>>>,
    sample_rate: u64,
    uuid: Uuid
}
impl QChannel {
    pub fn new(sample_rate: u64) -> (Self, QChannelX) {
        let (qch_tx, x_rx) = bounded_spsc_queue::make(10);
        let (x_tx, qch_rx) = bounded_spsc_queue::make(10);
        (QChannel {
            clients: Vec::with_capacity(100),
            rx: qch_rx,
            tx: qch_tx,
            sample_rate: sample_rate,
            uuid: Uuid::new_v4()
        }, QChannelX {
            uuid: Uuid::new_v4(),
            tx: x_tx,
            rx: x_rx,
            sent_uuids: Vec::new()
        })
    }
}

impl Source for QChannel {
    fn callback(&mut self, buffer: &mut [f32], frames: usize, _: bool) {
        if let Some(qcxr) = self.rx.try_pop() {
            match qcxr {
                QCXRequest::PushClient(cli) => self.clients.push(cli),
                QCXRequest::GetClient(uu) => {
                    let mut client_idx = None;
                    for (i, cli) in self.clients.iter().enumerate() {
                        if cli.uuid() == uu {
                            client_idx = Some(i);
                            break;
                        }
                    }
                    self.tx.push(if client_idx.is_none() {
                        None
                    }
                    else {
                        Some(self.clients.remove(client_idx.unwrap()))
                    })
                }
            }
        }
        for (i, client) in self.clients.iter_mut().enumerate() {
            client.callback(buffer, frames, i==0);
        }
    }
    fn sample_rate(&self) -> u64 {
        self.sample_rate
    }
    fn uuid(&self) -> Uuid {
        self.uuid.clone()
    }
}
