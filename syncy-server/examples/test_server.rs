use actix::{Actor, Addr};
use actix_http::StatusCode;
use actix_web::web;
use actix_web::{App, Error, HttpRequest, HttpResponse, HttpServer};
use actix_web_actors::ws;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use syncy_server::server::WsServer;
use syncy_server::session::WsSession;
use yrs::block::ClientID;

const PAYLOAD_SIZE_LIMIT: usize = 2 * 1024 * 1024; // 2MiB

#[actix::main]
async fn main() -> Result<(), actix_web::Error> {
    const PORT: u16 = 8080;
    env_logger::init();

    tracing::info!("Starting web server on port {}", PORT);
    let ws_server = WsServer::new().start();
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(ws_server.clone()))
            .app_data(web::PayloadConfig::new(PAYLOAD_SIZE_LIMIT))
            .route("/docs/", web::get().to(ws_handler))
    })
    .bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, PORT)))?
    .run()
    .await?;
    Ok(())
}

async fn ws_handler(
    req: HttpRequest,
    stream: web::Payload,
    server: web::Data<Addr<WsServer>>,
) -> Result<HttpResponse, Error> {
    let client_id = match get_client_id(&req) {
        None => return Ok(HttpResponse::new(StatusCode::BAD_REQUEST)),
        Some(client_id) => client_id,
    };
    let ws_server = server.get_ref().clone();
    tracing::trace!("accepting new session `{}`", client_id);
    ws::start(WsSession::new(client_id, ws_server), &req, stream)
}

fn get_client_id(req: &HttpRequest) -> Option<ClientID> {
    let header = req.headers().get("X-YRS-CLIENT-ID")?;
    let str = header.to_str().ok()?;
    str.parse().ok()
}
