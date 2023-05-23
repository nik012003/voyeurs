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
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{tcp::OwnedWriteHalf, TcpListener, TcpStream},
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
}

impl Shared {
    /// Create a new, empty, instance of `Shared`.
    fn new() -> Self {
        Shared {
            peers: HashMap::new(),
        }
    }
    async fn send_command(&mut self, addr: SocketAddr, command: VoyeursCommand, value: &str) {
        self.send(addr, &format!("{}:{}\n", command.to_string(), value))
            .await
    }

    async fn send(&mut self, addr: SocketAddr, message: &str) {
        self.peers
            .get_mut(&addr)
            .unwrap()
            .write_all(message.as_bytes())
            .await
            .unwrap();
    }

    async fn broadcast_command(&mut self, command: VoyeursCommand, value: &str) {
        self.broadcast(&format!("{}:{}\n", command.to_string(), value))
            .await
    }
    /// Send a `LineCodec` encoded message to every peer, except
    /// for the sender.
    async fn broadcast(&mut self, message: &str) {
        for peer in self.peers.iter_mut() {
            peer.1.write_all(message.as_bytes()).await.unwrap();
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
        let addr: SocketAddr = args
            .address
            .parse()
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
    let mut reader = BufReader::new(rx);
    state.lock().await.peers.insert(addr, tx);

    if !is_serving {
        if accept_source {
            state
                .lock()
                .await
                .send_command(addr, VoyeursCommand::StreamName, "")
                .await
        } else {
            while !matches!(mpv.event_listen().unwrap(), Event::FileLoaded) {}
            if !accept_source {
                state
                    .lock()
                    .await
                    .send_command(addr, VoyeursCommand::NewConnection, username)
                    .await
            }
        }
    }

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(_) => {
                if line.is_empty() {
                    println!("Disconnected peer");
                    state.lock().await.peers.remove(&addr).unwrap().forget();
                    // TODO: handle reconnection for clients
                    break;
                }

                match line_to_kv(&line) {
                    (VoyeursCommand::Pause, p) => {
                        mpv.set_property("pause", p.parse::<bool>().unwrap())
                            .unwrap();
                    }
                    (VoyeursCommand::Seek, t) => {
                        let current_time =
                            mpv.get_property_string("playback-time").unwrap_or_default();
                        if dbg!(t) != dbg!(current_time) {
                            mpv.seek(t.parse().unwrap_or_default(), SeekOptions::Absolute)
                                .unwrap()
                        }
                    }
                    (VoyeursCommand::NewConnection, username) => {
                        if !username.chars().all(char::is_alphanumeric) {
                            break;
                        }
                        mpv.pause().unwrap();
                        mpv.run_command_raw(
                            "show-text",
                            &vec![format!("{username}: connected").as_str(), "2000"],
                        )
                        .unwrap();

                        let filename = mpv.get_property_string("filename").unwrap_or_default();
                        let duration = mpv.get_property_string("duration").unwrap_or_default();
                        let pause = mpv.get_property_string("pause").unwrap_or_default();
                        let current_time =
                            mpv.get_property_string("playback-time").unwrap_or_default();
                        let mut s = state.lock().await;
                        s.send_command(addr, VoyeursCommand::Filename, &filename)
                            .await;
                        s.send_command(addr, VoyeursCommand::Duration, &duration)
                            .await;
                        s.send_command(addr, VoyeursCommand::Seek, &current_time)
                            .await;
                        s.send_command(addr, VoyeursCommand::Pause, &pause).await;
                    }
                    (VoyeursCommand::StreamName, "") => {
                        if is_serving {
                            // Check if path is a valid URL
                            // TODO: the correct way to check this is by using stream-open-filename and parsing its data

                            let mut streamname =
                                mpv.get_property_string("path").unwrap_or_default();
                            if Url::parse(&streamname).is_err() {
                                streamname = "".to_owned();
                            }
                            let mut s = state.lock().await;
                            s.send_command(addr, VoyeursCommand::StreamName, &streamname)
                                .await;
                        } else {
                            println!("Server is not streaming from a valid url")
                        }
                    }
                    (VoyeursCommand::StreamName, stream) => {
                        if accept_source {
                            mpv.run_command(MpvCommand::LoadFile {
                                file: stream.to_string(),
                                option: PlaylistAddOptions::Replace,
                            })
                            .unwrap();
                            while !matches!(mpv.event_listen().unwrap(), Event::FileLoaded) {}
                            let mut s = state.lock().await;
                            s.send_command(addr, VoyeursCommand::NewConnection, username)
                                .await
                        }
                    }
                    (VoyeursCommand::Filename, f) => {
                        if f != mpv.get_property::<String>("filename").unwrap_or_default() {
                            mpv.run_command_raw(
                                "show-text",
                                &vec!["filename does not match with server's filename", "2000"],
                            )
                            .unwrap();
                        }
                    }
                    (VoyeursCommand::Duration, t) => {
                        if dbg!(t) != dbg!(mpv.get_property_string("duration").unwrap_or_default())
                        {
                            mpv.run_command_raw(
                                "show-text",
                                &vec!["duration does not match with server's duration", "2000"],
                            )
                            .unwrap();
                        }
                    }
                    _ => println!("Unkown command"),
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
    loop {
        let event = mpv.event_listen().unwrap();
        match event {
            Event::Shutdown => exit(0),
            Event::EndFile => exit(0),
            Event::Seek => {}
            Event::PropertyChange { id: _, property } => match property {
                Property::Path(_) => todo!(),
                Property::Pause(p) => {
                    let cloned_state = Arc::clone(&state);
                    tokio::task::spawn(async move {
                        cloned_state
                            .lock()
                            .await
                            .broadcast_command(VoyeursCommand::Pause, p.to_string().as_str())
                            .await
                    });
                }
                Property::Unknown { name, data } => match name.as_str() {
                    "seeking" => {
                        println!("seeking:{:?}", data);
                        match data {
                            MpvDataType::Bool(false) => {
                                let cloned_state = Arc::clone(&state);
                                let current_time =
                                    mpv.get_property_string("playback-time").unwrap_or_default();
                                println!("myseek: {current_time}");
                                tokio::task::spawn(async move {
                                    cloned_state
                                        .lock()
                                        .await
                                        .broadcast_command(VoyeursCommand::Seek, &current_time)
                                        .await
                                });
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
