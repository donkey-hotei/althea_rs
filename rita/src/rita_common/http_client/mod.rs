//! Actor used for handling the dispatch of http messages, right now just hello messages
//!
//! The call path goes like this
//!
//! peer listener gets udp ImHere -> TunnelManager tries to contact peer with hello
//! -> http_client actually manages that request -> http_client calls back to tunnel manager

use tokio::net::TcpStream as TokioTcpStream;

use actix::prelude::*;
use actix::registry::SystemService;
use actix_web::*;

use futures::future::ok as future_ok;
use futures::Future;

use althea_types::LocalIdentity;

use rita_common::peer_listener::Peer;
use rita_common::tunnel_manager::{IdentityCallback, PortCallback, TunnelManager};

use actix_web::client::Connection;
use failure::Error;

#[derive(Default)]
pub struct HTTPClient;

impl Actor for HTTPClient {
    type Context = Context<Self>;
}

impl Supervised for HTTPClient {}
impl SystemService for HTTPClient {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {
        info!("HTTP Client started");
    }
}

#[derive(Debug)]
pub struct Hello {
    pub my_id: LocalIdentity,
    pub to: Peer,
}

impl Message for Hello {
    type Result = Result<(), Error>;
}

/// Handler for sending hello messages, it's important that any path by which this handler
/// may crash is handled such that ports are returned to tunnel manager, otherwise we end
/// up with a port leak which will eventually crash the program
impl Handler<Hello> for HTTPClient {
    type Result = ResponseFuture<(), Error>;
    fn handle(&mut self, msg: Hello, _: &mut Self::Context) -> Self::Result {
        trace!("Sending Hello {:?}", msg);

        let stream = TokioTcpStream::connect(&msg.to.contact_socket);

        let endpoint = format!(
            "http://[{}]:{}/hello",
            msg.to.contact_socket.ip(),
            msg.to.contact_socket.port()
        );

        Box::new(stream.then(move |stream| {
            trace!("stream status {:?}, to: {:?}", stream, &msg.to);
            let mut network_request = client::post(&endpoint);
            let peer = msg.to;
            let wg_port = msg.my_id.wg_port;

            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    trace!("Error getting stream from hello {:?}", e);
                    TunnelManager::from_registry().do_send(PortCallback(wg_port));
                    return Box::new(future_ok(())) as Box<Future<Item = (), Error = Error>>;
                }
            };

            let network_request = network_request.with_connection(Connection::from_stream(stream));

            let network_json = network_request.json(&msg.my_id);

            let network_json = match network_json {
                Ok(n) => n,
                Err(e) => {
                    trace!("Error serializing our request {:?}", e);
                    TunnelManager::from_registry().do_send(PortCallback(wg_port));
                    return Box::new(future_ok(())) as Box<Future<Item = (), Error = Error>>;
                }
            };

            trace!("sending hello request {:?}", network_json);

            let http_result = network_json.send().then(move |response| {
                trace!("got response from Hello {:?}", response);
                match response {
                    Ok(response) => Box::new(response.json().then(move |val| match val {
                        Ok(val) => {
                            TunnelManager::from_registry().do_send(IdentityCallback::new(
                                val,
                                peer,
                                Some(wg_port),
                            ));
                            Ok(())
                        }
                        Err(e) => {
                            trace!("Got error deserializing Hello {:?}", e);
                            TunnelManager::from_registry().do_send(PortCallback(wg_port));
                            Ok(())
                        }
                    }))
                        as Box<Future<Item = (), Error = Error>>,
                    Err(e) => {
                        trace!("Got error getting Hello response {:?}", e);
                        TunnelManager::from_registry().do_send(PortCallback(wg_port));
                        Box::new(future_ok(())) as Box<Future<Item = (), Error = Error>>
                    }
                }
            });

            Box::new(http_result) as Box<Future<Item = (), Error = Error>>
        }))
    }
}
