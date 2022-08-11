#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

#[cfg(target_os = "none")]
extern crate pal_nrf as pal;

#[cfg(target_os = "linux")]
extern crate pal_pc as pal;

#[macro_use]
extern crate alloc;

#[macro_use]
extern crate log;

#[macro_use]
extern crate async_trait;

#[macro_use]
extern crate pin_project;

use alloc::boxed::Box;
use alloc::string::ToString;
use pal::embassy_net::{udp::UdpSocket, PacketMetadata};

use coap_server::app::{CoapError, Request, Response};
use coap_server::{app, CoapServer};
use pal::embassy_util::Forever;
use trussed::ClientImplementation;

mod rng;
mod udp;

#[pal::main]
async fn main() {
    info!("Hello from main");
    let mut clients = pal::trussed::init(&["fobnail"]);

    static TRUSSED: Forever<ClientImplementation<pal::trussed::Syscall>> = Forever::new();
    let trussed = TRUSSED.put(clients.pop().unwrap());

    let stack = pal::net::stack();
    let mut rx_meta = [PacketMetadata::EMPTY; 16];
    let mut rx_buffer = [0; 4096];
    let mut tx_meta = [PacketMetadata::EMPTY; 16];
    let mut tx_buffer = [0; 4096];
    let mut buf = [0; 4096];

    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    socket.bind(9400).unwrap();

    static RX_META: Forever<[PacketMetadata; 16]> = Forever::new();
    let rx_meta = RX_META.put([PacketMetadata::EMPTY; 16]);
    static RX_BUFFER: Forever<[u8; 4096]> = Forever::new();
    let rx_buffer = RX_BUFFER.put([0; 4096]);
    static TX_META: Forever<[PacketMetadata; 16]> = Forever::new();
    let tx_meta = TX_META.put([PacketMetadata::EMPTY; 16]);
    static TX_BUFFER: Forever<[u8; 4096]> = Forever::new();
    let tx_buffer = TX_BUFFER.put([0; 4096]);

    /*let server = CoapServer::bind(udp::UdpTransport::new("0.0.0.0:5683"))
    .await
    .unwrap();*/
    let server = CoapServer::bind(udp::UdpTransport::new(
        UdpSocket::new(stack, rx_meta, rx_buffer, tx_meta, tx_buffer),
        5683,
    ))
    .await
    .unwrap();
    server
        .serve(
            app::new()
                .resource(
                    app::resource("/hello")
                        // Try `coap-client -m get coap://localhost/.well-known/core` to see this!
                        //                        .link_attr(LINK_ATTR_RESOURCE_TYPE, "hello")
                        .get(handle_get_hello),
                )
                .resource(
                    app::resource("/hidden")
                        .not_discoverable()
                        .get(handle_get_hidden),
                ),
            Box::new(rng::TrussedRng(trussed)),
        )
        .await
        .unwrap();

    loop {
        let (n, ep) = socket.recv_from(&mut buf).await.unwrap();
        if let Ok(s) = core::str::from_utf8(&buf[..n]) {
            info!("ECHO (to {}): {}", ep, s);
        } else {
            info!("ECHO (to {}): bytearray len {}", ep, n);
        }
        socket.send_to(&buf[..n], ep).await.unwrap();
    }
}

async fn handle_get_hello(
    request: Request<pal::embassy_net::IpAddress>,
) -> Result<Response, CoapError> {
    let whom = request
        .unmatched_path
        .first()
        .cloned()
        .unwrap_or_else(|| "world".to_string());

    let mut response = request.new_response();
    response.message.payload = format!("Hello, {whom}").into_bytes();
    Ok(response)
}

async fn handle_get_hidden(
    request: Request<pal::embassy_net::IpAddress>,
) -> Result<Response, CoapError> {
    let mut response = request.new_response();
    response.message.payload = b"sshh!".to_vec();
    Ok(response)
}
