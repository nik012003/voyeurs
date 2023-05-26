mod client_message_handler;
mod mpv_event_handler;
mod proto;

use clap::Parser;
use client_message_handler::*;
use mpv_event_handler::*;
use mpvipc::*;
use proto::*;
use std::net::SocketAddr;
use std::vec;
use std::{collections::HashMap, process::Command, sync::Arc};
use tempfile::tempdir;
use tokio::{
    io::AsyncWriteExt,
    net::{lookup_host, tcp::OwnedWriteHalf, TcpListener, TcpStream},
    sync::Mutex,
};

#[derive(Parser)]
#[command(about, version)]
struct Cli {
    /// become a server, if not set you are a client
    #[arg(short, long)]
    serve: bool,

    /// playback the uri got from the server
    #[arg(short, long, conflicts_with = "serve")]
    accept_source: bool,

    /// username that will be sent to the server
    #[arg(short, long, default_value = "user")]
    username: String,

    /// address:port to connect/bind to  
    #[arg(value_name = "ADDRESS")]
    address: String,

    // arguments that will get passed to mpv
    #[arg(value_name = "MPV_ARGS")]
    mpv_args: Vec<String>,
}

pub struct Peer {
    tx: OwnedWriteHalf,
    username: String,
    ready: bool,
}

pub struct Shared {
    peers: HashMap<SocketAddr, Peer>,
    ignore_next: bool,
    is_ready: bool,
}

impl Shared {
    fn new() -> Self {
        Shared {
            peers: HashMap::new(),
            ignore_next: false,
            is_ready: false,
        }
    }

    async fn send(&mut self, addr: SocketAddr, command: VoyeursCommand) {
        self.peers
            .get_mut(&addr)
            .unwrap()
            .tx
            .write_all(&command.craft_packet().compile())
            .await
            .unwrap();
    }

    async fn broadcast(&mut self, command: VoyeursCommand) {
        dbg!(&command);
        for peer in self.peers.iter_mut() {
            peer.1
                .tx
                .write_all(&command.clone().craft_packet().compile())
                .await
                .unwrap();
        }
    }

    async fn broadcast_excluding(&mut self, command: VoyeursCommand, addr: SocketAddr) {
        dbg!(&command, addr);
        for peer in self.peers.iter_mut() {
            if *peer.0 != addr {
                peer.1
                    .tx
                    .write_all(&command.clone().craft_packet().compile())
                    .await
                    .unwrap();
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let args = Cli::parse();

    // generate temp path for the socket
    let binding = tempdir()
        .expect("Failed to create a tmp directory for the mpv socket")
        .into_path()
        .join("mpv.sock");
    let mpv_socket = binding.to_str().unwrap().to_owned();

    // start mpv
    let mut gui_mode_args = vec![];
    if args.accept_source {
        gui_mode_args.push("--player-operation-mode=pseudo-gui".to_string());
    }

    Command::new("mpv")
        .arg(format!("--input-ipc-server={}", mpv_socket))
        .args(gui_mode_args)
        .args(args.mpv_args)
        .spawn()
        .expect("failed to execute mpv");

    // enstabilish a connection to the mpv socket
    let mut mpv: Result<Mpv, Error> = Err(mpvipc::Error(mpvipc::ErrorCode::ConnectError(
        "Not yet connected".to_owned(),
    )));
    while mpv.is_err() {
        mpv = Mpv::connect(mpv_socket.as_str());
    }
    let mpv = mpv.unwrap();

    // setup necessary property observers

    mpv.observe_property(0, "pause").unwrap();
    mpv.observe_property(1, "seeking").unwrap();

    mpv.run_command_raw("show-text", &vec!["Connected to voyeurs", "5000"])
        .unwrap();

    let state = Arc::new(Mutex::new(Shared::new()));

    let cloned_state = Arc::clone(&state);
    tokio::task::spawn_blocking(move || handle_mpv_event(mpv, cloned_state));

    // Handle server
    if args.serve {
        let listener = TcpListener::bind(&args.address)
            .await
            .expect(&format!("Couldn't bind address to {}", args.address));
        println!("Starting server on {}", args.address);
        loop {
            let (stream, addr) = listener.accept().await.unwrap();
            // Asynchronously wait for an inbound TcpStream.

            // Clone a handle to the `Shared` state for the new connection.
            let state: Arc<Mutex<Shared>> = Arc::clone(&state);

            let mpv =
                Mpv::connect(mpv_socket.as_str()).expect("Task coudln't attach to mpv socket");

            // Spawn our handler to be run asynchronously.
            tokio::spawn(async move {
                handle_connection(mpv, addr, stream, state, true, "server", false).await
            });
        }
    }
    // Handle client
    else {
        println!("Connecting to {}", args.address);
        let addr = lookup_host(args.address)
            .await
            .expect("Server lookup failed")
            .next()
            .expect("Couldn't create SockAddr from the given address and port");

        let stream = TcpStream::connect(addr)
            .await
            .expect("Could not connect to server");
        let mpv = Mpv::connect(mpv_socket.as_str()).expect("Task coudln't attach to mpv socket");

        let communication_task = tokio::spawn(async move {
            handle_connection(
                mpv,
                addr,
                stream,
                state,
                false,
                &args.username,
                args.accept_source,
            )
            .await
        });
        let _ = tokio::join!(communication_task);
    }
}
