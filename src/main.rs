mod proto;
use clap::Parser;
use mpvipc::*;
use proto::*;
use std::net::SocketAddr;
use std::vec;
use std::{
    collections::HashMap,
    process::{exit, Command},
    sync::Arc,
};
use tempfile::tempdir;
use tokio::runtime::Runtime;
use tokio::{
    io::AsyncWriteExt,
    net::{lookup_host, tcp::OwnedWriteHalf, TcpListener, TcpStream},
    sync::Mutex,
};
use url::Url;

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

struct Shared {
    peers: HashMap<SocketAddr, OwnedWriteHalf>,
    _ready_peers: Vec<bool>,
    ignore_next: bool,
}

impl Shared {
    /// Create a new, empty, instance of `Shared`.
    fn new() -> Self {
        Shared {
            peers: HashMap::new(),
            _ready_peers: vec![],
            ignore_next: false,
        }
    }

    async fn send(&mut self, addr: SocketAddr, command: VoyeursCommand) {
        self.peers
            .get_mut(&addr)
            .unwrap()
            .write_all(&command.craft_packet().compile())
            .await
            .unwrap();
    }

    /// Send a `LineCodec` encoded message to every peer, except
    /// for the sender.
    async fn broadcast(&mut self, command: VoyeursCommand) {
        for peer in self.peers.iter_mut() {
            peer.1
                .write_all(&command.clone().craft_packet().compile())
                .await
                .unwrap();
        }
    }

    async fn broadcast_excluding(&mut self, command: VoyeursCommand, addr: SocketAddr) {
        for peer in self.peers.iter_mut() {
            if *peer.0 != addr {
                peer.1
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
    tokio::task::spawn_blocking(move || process_mpv_event(mpv, cloned_state));

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

async fn handle_connection(
    mut mpv: Mpv,
    addr: SocketAddr,
    stream: TcpStream,
    state: Arc<Mutex<Shared>>,
    is_serving: bool,
    username: &str,
    accept_source: bool,
) {
    println!("accepted connection");
    let (rx, tx) = stream.into_split();
    let reader = PacketReader::new(rx);
    state.lock().await.peers.insert(addr, tx);

    if !is_serving {
        if accept_source {
            state
                .lock()
                .await
                .send(addr, VoyeursCommand::GetStreamName)
                .await
        } else {
            while !matches!(mpv.event_listen().unwrap(), Event::FileLoaded) {}
            if !accept_source {
                state
                    .lock()
                    .await
                    .send(addr, VoyeursCommand::NewConnection(username.to_string()))
                    .await
            }
        }
    }

    loop {
        match reader.read_packet().await {
            Ok(packet) => {
                match dbg!(packet.command) {
                    VoyeursCommand::Pause(p) => {
                        let mut s = state.lock().await;
                        s.ignore_next = true;
                        mpv.set_property("pause", p).unwrap();
                        if is_serving {
                            s.broadcast_excluding(VoyeursCommand::Pause(p), addr).await;
                        }
                    }
                    VoyeursCommand::Seek(t) => {
                        let current_time: f64 =
                            mpv.get_property("playback-time").unwrap_or_default();
                        let mut s = state.lock().await;
                        if t != current_time {
                            s.ignore_next = true;

                            mpv.seek(t, SeekOptions::Absolute).unwrap();

                            if is_serving {
                                s.broadcast_excluding(VoyeursCommand::Seek(t), addr).await;
                            }
                        }
                    }
                    VoyeursCommand::NewConnection(username) => {
                        if !username.chars().all(char::is_alphanumeric) {
                            break;
                        }
                        mpv.pause().unwrap();
                        mpv.run_command_raw(
                            "show-text",
                            &vec![format!("{username}: connected").as_str(), "2000"],
                        )
                        .unwrap();

                        let filename = mpv.get_property("filename").unwrap_or_default();
                        let duration = mpv.get_property("duration").unwrap_or_default();
                        let pause = mpv.get_property("pause").unwrap_or_default();
                        let current_time = mpv.get_property("playback-time").unwrap_or_default();
                        let mut s = state.lock().await;
                        s.send(addr, VoyeursCommand::Filename(filename)).await;
                        s.send(addr, VoyeursCommand::Duration(duration)).await;
                        s.send(addr, VoyeursCommand::Seek(current_time)).await;
                        s.send(addr, VoyeursCommand::Pause(pause)).await;
                    }
                    VoyeursCommand::GetStreamName => {
                        if is_serving {
                            // Check if path is a valid URL
                            // TODO: the correct way to check this is by using stream-open-filename and parsing its data

                            let mut streamname =
                                mpv.get_property_string("path").unwrap_or_default();
                            if Url::parse(&streamname).is_err() {
                                streamname = "".to_owned();
                            }
                            let mut s = state.lock().await;
                            s.send(addr, VoyeursCommand::StreamName(streamname)).await;
                        }
                    }
                    VoyeursCommand::StreamName(stream) => {
                        if accept_source {
                            if stream.is_empty() {
                                println!("Server is not streaming from a valid url")
                            }
                            mpv.run_command(MpvCommand::LoadFile {
                                file: stream.to_string(),
                                option: PlaylistAddOptions::Replace,
                            })
                            .unwrap();
                            while !matches!(mpv.event_listen().unwrap(), Event::FileLoaded) {}
                            let mut s = state.lock().await;
                            s.send(addr, VoyeursCommand::NewConnection(username.to_string()))
                                .await
                        }
                    }
                    VoyeursCommand::Filename(f) => {
                        if f != mpv.get_property::<String>("filename").unwrap_or_default() {
                            mpv.run_command_raw(
                                "show-text",
                                &vec!["filename does not match with server's filename", "2000"],
                            )
                            .unwrap();
                        }
                    }
                    VoyeursCommand::Duration(t) => {
                        if t != mpv.get_property::<f64>("duration").unwrap_or_default() {
                            mpv.run_command_raw(
                                "show-text",
                                &vec!["duration does not match with server's duration", "2000"],
                            )
                            .unwrap();
                        }
                    }
                }
            }
            Err(_) => {
                println!("Disconnected peer");
                state.lock().await.peers.remove(&addr).unwrap().forget();
                break;
            }
        };
    }
}

fn process_mpv_event(mut mpv: Mpv, state: Arc<Mutex<Shared>>) {
    let rt = Runtime::new().unwrap();
    let handle = rt.handle();
    loop {
        let event = mpv.event_listen().unwrap();
        let mut s = handle.block_on(state.lock());
        if dbg!(s.ignore_next) {
            s.ignore_next = false;
            continue;
        }
        match event {
            Event::Shutdown => exit(0),
            Event::EndFile => exit(0),
            Event::PropertyChange { id: _, property } => match property {
                Property::Path(_) => todo!(),
                Property::Pause(p) => {
                    handle.block_on(s.broadcast(VoyeursCommand::Pause(p)));
                }
                Property::Unknown { name, data } => match name.as_str() {
                    "seeking" => {
                        println!("seeking:{:?}", data);
                        match data {
                            MpvDataType::Bool(false) => {
                                let current_time =
                                    mpv.get_property("playback-time").unwrap_or_default();
                                handle.block_on(s.broadcast(VoyeursCommand::Seek(current_time)));
                            }
                            MpvDataType::Bool(true) => {
                                println!("Houston we have a buffering problem");
                            }
                            _ => {}
                        }
                    }
                    _ => todo!(),
                },
                _ => todo!(),
            },
            _ => {}
        }
    }
}
