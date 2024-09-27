use super::server::{ClientMessage, Join, Leave, WsServer};
use actix::{
    fut, Actor, ActorContext, ActorFutureExt, Addr, AsyncContext, ContextFutureSpawner, Handler,
    Running, StreamHandler, WrapFuture,
};
use actix_http::ws::{CloseCode, CloseReason, Item, ProtocolError};
use actix_web_actors::ws;
use bytes::{Bytes, BytesMut};
use std::time::{Duration, Instant};
use yrs::block::ClientID;

pub const HEARTBEAT: Duration = Duration::from_secs(5);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct WsSession {
    id: ClientID,
    server: Addr<WsServer>,
    hb: Instant,
    buf: Option<BytesMut>,
}

impl WsSession {
    pub fn new(id: ClientID, server: Addr<WsServer>) -> Self {
        WsSession {
            id,
            server,
            hb: Instant::now(),
            buf: None,
        }
    }

    fn hb(&self, ctx: &mut ws::WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT, |act, ctx| {
            if Instant::now().duration_since(act.hb) > CLIENT_TIMEOUT {
                tracing::trace!(
                    "session `{}` failed to receive pong within {:?}",
                    act.id,
                    CLIENT_TIMEOUT
                );
                act.server.do_send(Leave { session_id: act.id });
                ctx.stop();
                return;
            }
            ctx.ping(b"");
        });
    }

    fn handle_protocol(&mut self, bytes: Bytes, ctx: &mut ws::WebsocketContext<Self>) {
        tracing::trace!("session `{}` broadcasting {} bytes", self.id, bytes.len());
        let fut = self
            .server
            .send(ClientMessage {
                sender: self.id,
                data: bytes,
            })
            .into_actor(self)
            .map(|res, act, _| {
                if let Err(err) = res {
                    tracing::warn!("session `{}`failed to broadcast message: {}", act.id, err);
                }
            });
        ctx.wait(fut);
    }
}

impl Actor for WsSession {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.hb(ctx);
        let join = Join {
            session_id: self.id.clone(),
            addr: ctx.address().recipient(),
            docs: vec![],
        };
        self.server
            .send(join)
            .into_actor(self)
            .then(|res, act, ctx| {
                if let Err(err) = res {
                    tracing::warn!("session `{}` can't join: {}", act.id, err);
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);
    }

    fn stopping(&mut self, _ctx: &mut Self::Context) -> Running {
        tracing::trace!("stopping session `{}`", self.id);
        self.server.do_send(Leave {
            session_id: self.id,
        });
        Running::Stop
    }
}

impl Handler<Message> for WsSession {
    type Result = ();

    fn handle(&mut self, msg: Message, ctx: &mut Self::Context) {
        tracing::trace!(
            "sending message through session `{}`: {} bytes",
            self.id,
            msg.0.len()
        );
        ctx.binary(msg.0);
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsSession {
    fn handle(&mut self, item: Result<ws::Message, ProtocolError>, ctx: &mut Self::Context) {
        match item {
            Ok(message) => {
                self.hb = Instant::now();
                match message {
                    ws::Message::Ping(bytes) => {
                        ctx.pong(&bytes);
                    }
                    ws::Message::Pong(_) => {}
                    ws::Message::Text(_) => ctx.close(Some(CloseReason::from((
                        CloseCode::Unsupported,
                        "only binary content is supported",
                    )))),
                    ws::Message::Binary(bytes) => {
                        self.handle_protocol(bytes, ctx);
                    }
                    ws::Message::Continuation(item) => {
                        match item {
                            Item::FirstText(_) => ctx.close(Some(CloseReason::from((
                                CloseCode::Unsupported,
                                "only binary content is supported",
                            )))),
                            Item::FirstBinary(bytes) => self.buf = Some(bytes.into()),
                            Item::Continue(bytes) => {
                                if let Some(ref mut buf) = self.buf {
                                    buf.extend_from_slice(&bytes);
                                } else {
                                    ctx.close(Some(CloseReason::from((
                                        CloseCode::Protocol,
                                        "continuation frame without initialization",
                                    )))); // unexpected
                                }
                            }
                            Item::Last(bytes) => {
                                if let Some(mut buf) = self.buf.take() {
                                    buf.extend_from_slice(&bytes);
                                    self.handle_protocol(buf.freeze(), ctx);
                                } else {
                                    ctx.close(Some(CloseReason::from((
                                        CloseCode::Protocol,
                                        "last frame without initialization",
                                    )))); // unexpected
                                }
                            }
                        }
                    }
                    ws::Message::Close(reason) => {
                        ctx.close(reason);
                        ctx.stop();
                    }
                    _ => { /* do nothing */ }
                }
            }
            Err(err) => {
                tracing::warn!("session `{}` websocket protocol error: {}", self.id, err);
                ctx.stop();
            }
        }
    }
}

#[derive(actix::Message)]
#[rtype(result = "()")]
pub struct Message(pub Bytes);
