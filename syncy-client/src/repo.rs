use bytes::BytesMut;
use dashmap::{DashMap, Entry};
use futures_sink::Sink;
use futures_util::stream::StreamExt;
use futures_util::{SinkExt, Stream};
use std::sync::{Arc, Weak};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Uri};
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;
use yrs::block::ClientID;
use yrs::encoding::read::Cursor;
use yrs::encoding::write::Write;
use yrs::sync::{Awareness, MessageReader, SyncMessage};
use yrs::updates::decoder::{Decode, DecoderV1};
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{AsyncTransact, Doc, OffsetKind, Origin, ReadTxn, Update};

pub const MAX_FRAME_SIZE: usize = 128 * 1024; // 128KiB

pub struct DocRepo {
    url: Uri,
    client_id: ClientID,
    outbound: UnboundedSender<Message>,
    docs: Arc<DashMap<Uuid, Doc>>,
}

impl DocRepo {
    pub async fn new<R>(req: R, client_id: ClientID) -> crate::Result<Self>
    where
        R: IntoClientRequest,
    {
        let mut config = WebSocketConfig::default();
        config.max_frame_size = None;

        let mut req = req.into_client_request()?;
        let url = req.uri().clone();
        req.headers_mut()
            .append("X-YRS-CLIENT-ID", HeaderValue::from(client_id));
        let (ws_stream, _resp) = connect_async_with_config(req, Some(config), false).await?;
        // channel to send data to the server
        let (outbound_tx, outbound_rx) = unbounded_channel();
        let (sink, stream) = ws_stream.split();

        // spawn outbound request handler
        tokio::spawn(async move {
            if let Err(err) = Self::outbound(outbound_rx, sink).await {
                tracing::error!("client outbound handler failed: {}", err);
            }
        });

        // spawn inbound response handler
        let docs = Arc::new(DashMap::new());
        let outbound = outbound_tx.clone();
        let weak_docs = Arc::downgrade(&docs);
        tokio::spawn(async move {
            if let Err(err) = Self::inbound(client_id, stream, weak_docs, outbound).await {
                tracing::error!("client inbound handler failed: {}", err);
            }
        });
        Ok(Self {
            url,
            client_id,
            outbound: outbound_tx,
            docs,
        })
    }

    pub fn url(&self) -> &Uri {
        &self.url
    }

    pub fn doc(&self, doc_id: Uuid) -> crate::Result<Doc> {
        match self.docs.entry(doc_id) {
            Entry::Occupied(e) => Ok(e.get().clone()),
            Entry::Vacant(e) => {
                let doc = Self::init_doc(self.client_id, doc_id, self.outbound.clone());
                Ok(e.insert(doc).value().clone())
            }
        }
    }

    async fn observe_awareness(awareness: &Arc<Awareness>, outbound: OutboundSink) {
        let doc = awareness.doc();
        let my_origin: Origin = doc.client_id().into();
        // send initial state of the document to the server
        outbound.send(yrs::sync::Message::Awareness(awareness.update().unwrap()));

        // subscribe to awareness changes
        {
            let my_origin = my_origin.clone();
            let outbound = outbound.clone();
            awareness.on_update_with("dr", move |awareness, e, origin| {
                if origin == Some(&my_origin) {
                    let msg = yrs::sync::Message::Awareness(
                        awareness.update_with_clients(e.all_changes()).unwrap(),
                    );
                    outbound.send(msg);
                }
            });
        }

        Self::observe_doc(doc, outbound);
    }

    fn observe_doc(doc: &Doc, outbound: OutboundSink) {
        let my_origin: Origin = doc.client_id().into();
        let state_vector = yrs::Transact::transact(doc).state_vector();
        let msg = yrs::sync::Message::Sync(SyncMessage::SyncStep1(state_vector));
        outbound.send(msg);

        // subscribe to document updates
        doc.observe_update_v1_with("dr", move |txn, e| {
            if txn.origin() == Some(&my_origin) {
                let msg = yrs::sync::Message::Sync(SyncMessage::Update(e.update.clone()));
                outbound.send(msg);
            }
        })
        .unwrap();
    }

    async fn inbound<S>(
        client_id: ClientID,
        mut stream: S,
        docs: Weak<DashMap<Uuid, Doc>>,
        outbound_tx: UnboundedSender<Message>,
    ) -> crate::Result<()>
    where
        S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        let mut buf = BytesMut::new();
        while let Some(result) = stream.next().await {
            let message = result?;
            let docs = match docs.upgrade() {
                None => return Ok(()), // controller has been dropped
                Some(docs) => docs,
            };
            match message {
                Message::Text(_) => {
                    return Err(crate::Error::Unsupported(
                        "text payload is not supported".into(),
                    ))
                }
                Message::Binary(bytes) => {
                    Self::handle_inbound_message(client_id, &docs, &bytes, &outbound_tx).await?
                }
                Message::Ping(bytes) => {
                    let _ = outbound_tx.send(Message::Pong(bytes));
                }
                Message::Pong(_) => { /* do nothing */ }
                Message::Close(reason) => {
                    tracing::info!("client closed: {:?}", reason);
                    return Ok(());
                }
                Message::Frame(frame) => {
                    let header = frame.header();
                    buf.extend_from_slice(frame.payload().as_ref());
                    if header.is_final {
                        let bytes = std::mem::take(&mut buf);
                        tracing::trace!("finalized split frames: {} bytes", bytes.len());
                        Self::handle_inbound_message(client_id, &docs, &bytes, &outbound_tx).await?
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_inbound_message(
        client_id: ClientID,
        docs: &DashMap<Uuid, Doc>,
        bytes: &[u8],
        outbound_tx: &UnboundedSender<Message>,
    ) -> crate::Result<()> {
        let doc_id = Uuid::from_slice(&bytes[..16])?;
        let body = &bytes[16..];
        let mut decoder = DecoderV1::new(Cursor::new(body));
        let reader = MessageReader::new(&mut decoder);
        let doc = match docs.entry(doc_id) {
            Entry::Occupied(e) => e.get().clone(),
            Entry::Vacant(e) => {
                let doc = Self::init_doc(client_id, doc_id, outbound_tx.clone());
                e.insert(doc).value().clone()
            }
        };
        for result in reader {
            let msg = result?;
            tracing::trace!("client {} handling doc `{}` message", client_id, doc_id);
            match msg {
                yrs::sync::Message::Sync(SyncMessage::SyncStep1(sv)) => {
                    let tx = doc.transact().await;
                    let mut encoder = EncoderV1::new();
                    encoder.write_all(doc_id.as_bytes().as_slice());
                    let bytes = tx.encode_state_as_update_v1(&sv);
                    let msg = yrs::sync::Message::Sync(SyncMessage::SyncStep2(bytes));
                    msg.encode(&mut encoder);
                    let _ = outbound_tx.send(Message::Binary(encoder.to_vec()));
                }
                yrs::sync::Message::Sync(SyncMessage::SyncStep2(bytes)) => {
                    let update = Update::decode_v1(&bytes)?;
                    let mut tx = doc.transact_mut().await;
                    tx.apply_update(update)?;
                }
                yrs::sync::Message::Sync(SyncMessage::Update(bytes)) => {
                    let update = Update::decode_v1(&bytes)?;
                    let mut tx = doc.transact_mut().await;
                    tx.apply_update(update)?;
                }
                yrs::sync::Message::Auth(deny_reason) => {
                    tracing::warn!(
                        "client {} action denied for doc `{}`: {:?}",
                        client_id,
                        doc_id,
                        deny_reason
                    )
                }
                yrs::sync::Message::AwarenessQuery => {
                    tracing::trace!("client {} received awareness query", client_id)
                }
                yrs::sync::Message::Awareness(_awareness_update) => {
                    tracing::trace!("client {} received awareness update", client_id)
                }
                yrs::sync::Message::Custom(tag, _) => {
                    tracing::warn!("client {} got unknown message - tag: {}", client_id, tag)
                }
            }
        }
        Ok(())
    }

    fn init_doc(client_id: ClientID, doc_id: Uuid, outbound_tx: UnboundedSender<Message>) -> Doc {
        tracing::trace!("client `{}` init new doc: {}", client_id, doc_id);
        let doc = Doc::with_options(yrs::Options {
            client_id,
            guid: doc_id.to_string().into(),
            collection_id: None,
            offset_kind: OffsetKind::Bytes,
            skip_gc: false,
            auto_load: true,
            should_load: true,
        });
        let outbound = OutboundSink {
            doc_id,
            outbound: outbound_tx,
        };
        Self::observe_doc(&doc, outbound);
        doc
    }

    async fn outbound<S>(
        mut outbound_rx: UnboundedReceiver<Message>,
        mut sink: S,
    ) -> crate::Result<()>
    where
        S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        while let Some(msg) = outbound_rx.recv().await {
            let len = msg.len();
            if len <= MAX_FRAME_SIZE {
                tracing::trace!("sending message: {} bytes", len);
                sink.send(msg).await?;
            } else if let Message::Binary(bytes) = msg {
                tracing::trace!("sending message: {} bytes (frame split)", len);
                Self::send_as_frames(&mut sink, bytes).await?;
            } else {
                tracing::warn!("non-binary message too big to be send: {} bytes", len)
            }
        }
        Ok(())
    }

    async fn send_as_frames<S>(sink: &mut S, data: Vec<u8>) -> crate::Result<()>
    where
        S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        let mut consumed = 0;
        while consumed < data.len() {
            let op_code = if consumed == 0 {
                OpCode::Data(Data::Binary)
            } else {
                OpCode::Data(Data::Continue)
            };
            let mut end = consumed + MAX_FRAME_SIZE;
            let mut is_final = false;
            if end >= data.len() {
                end = data.len();
                is_final = true;
            }

            let slice = &data[consumed..end];
            let frame = Frame::message(slice.into(), op_code, is_final);
            sink.send(Message::Frame(frame)).await?;
            consumed = end;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct OutboundSink {
    doc_id: Uuid,
    outbound: UnboundedSender<Message>,
}

impl OutboundSink {
    fn send(&self, msg: yrs::sync::Message) {
        let mut encoder = EncoderV1::new();
        encoder.write_all(self.doc_id.as_bytes());
        msg.encode(&mut encoder);

        let _ = self.outbound.send(Message::Binary(encoder.to_vec()));
    }
}
