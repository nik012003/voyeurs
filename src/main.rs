mod client_message_handler;
mod mpv_event_handler;
mod proto;

use clap::Parser;
use client_message_handler::*;
use mpv_event_handler::*;
use mpvipc::*;
use proto::*;
use rsntp::SntpClient;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};
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

    /// username that will be sent to the server
    #[arg(long)]
    standalone: bool,

    /// use system time instead of ntp (not reccomended)
    #[arg(short, long)]
    trust_system_time: bool,

    /// address of the ntp server
    #[arg(
        long,
        conflicts_with = "trust_system_time",
        default_value = "pool.ntp.org"
    )]
    ntp_server: String,

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
#[derive(Clone, Debug)]
pub struct Settings {
    is_serving: bool,
    username: String,
    accept_source: bool,
    standalone: bool,
}

#[tokio::main]
async fn main() {
    let args = Cli::parse();

    if !args.trust_system_time {
        let client = SntpClient::new();
        let result = client
            .synchronize(args.ntp_server)
            .expect("Coudn't syncronize time with ntp server");
        let delta: i64 = result.datetime().unix_timestamp().expect("msg").as_millis() as i64
            - SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Couldn't get system time")
                .as_millis() as i64;
        println!("Clock skew: {} ms", delta);
        TIME_DELTA.store(delta, std::sync::atomic::Ordering::SeqCst);
    }

    let state = Arc::new(Mutex::new(Shared::new()));

    let cloned_state = Arc::clone(&state);
    let mpv_socket =
        start_mpv(args.accept_source, args.mpv_args).expect("Coudln't start or connect to mpv");

    let settings = Settings {
        is_serving: args.serve,
        username: args.username,
        accept_source: args.accept_source,
        standalone: args.standalone,
    };

    // Handle server
    if args.serve {
        let listener = TcpListener::bind(&args.address)
            .await
            .expect("Couldn't bind address");
        println!("Starting server on {}", args.address);
        let mpv = Mpv::connect(mpv_socket.as_str()).expect("Task coudln't attach to mpv socket");
        tokio::task::spawn_blocking(move || handle_mpv_event(mpv, cloned_state, args.standalone));
        loop {
            let (stream, addr) = listener.accept().await.unwrap();
            // Asynchronously wait for an inbound TcpStream.

            // Clone a handle to the `Shared` state for the new connection.
            let state: Arc<Mutex<Shared>> = Arc::clone(&state);

            let mpv =
                Mpv::connect(mpv_socket.as_str()).expect("Task coudln't attach to mpv socket");

            // Spawn our handler to be run asynchronously.
            let cloned_settings = settings.clone();
            tokio::spawn(async move {
                handle_connection(mpv, addr, stream, state, cloned_settings).await
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
        let communication_task =
            tokio::spawn(
                async move { handle_connection(mpv, addr, stream, state, settings).await },
            );
        let mpv = Mpv::connect(mpv_socket.as_str()).expect("Task coudln't attach to mpv socket");
        tokio::task::spawn_blocking(move || handle_mpv_event(mpv, cloned_state, args.standalone));
        let _ = tokio::join!(communication_task);
    }
}

fn start_mpv(accept_source: bool, mpv_args: Vec<String>) -> Result<String, Error> {
    // generate temp path for the socket
    let binding = tempdir()
        .expect("Failed to create a tmp directory for the mpv socket")
        .into_path()
        .join("mpv.sock");
    let mpv_socket = binding
        .to_str()
        .expect("Path isn't valid unicode")
        .to_owned();

    // start mpv
    let mut gui_mode_args = vec![];
    if accept_source {
        gui_mode_args.push("--player-operation-mode=pseudo-gui".to_string());
    }

    Command::new("mpv")
        .arg(format!("--input-ipc-server={}", mpv_socket))
        .args(gui_mode_args)
        .args(mpv_args)
        .spawn()
        .expect("failed to execute mpv");

    // enstabilish a connection to the mpv socket
    let mut mpv: Result<Mpv, Error> = Err(mpvipc::Error(mpvipc::ErrorCode::ConnectError(
        "Not yet connected".to_owned(),
    )));
    while mpv.is_err() {
        mpv = Mpv::connect(mpv_socket.as_str());
    }
    let mpv = mpv?;
    mpv.pause()?;

    mpv.run_command_raw("show-text", &["Connected to voyeurs", "5000"])?;

    Ok(mpv_socket)
}
