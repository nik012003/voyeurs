use std::{
    collections::HashMap,
    process::{exit, Command},
    sync::Arc,
};

use clap::Parser;
use mpvipc::*;
use std::net::SocketAddr;
use tempfile::tempdir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{tcp::OwnedWriteHalf, TcpListener, TcpStream},
    sync::Mutex,
};

#[derive(Parser)]
#[command(about, version)]
struct Cli {
    /// become a server, if not set you are a client
    #[arg(short, long)]
    serve: bool,

    /// address:port to connect/bind to  
    #[arg(value_name = "ADDRESS")]
    address: String,

    // arguments that will get passed to mpv
    #[arg(value_name = "MPV_ARGS")]
    mpv_args: Vec<String>,
}

#[repr(isize)]
enum MpvProps {
    PAUSE = 1,
    PLAYBACKTIME = 2,
    SEEKING = 3,
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

    async fn send(&mut self, addr: SocketAddr, message: &str) {
        self.peers
            .get_mut(&addr)
            .unwrap()
            .write_all(message.as_bytes())
            .await
            .unwrap();
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
    Command::new("mpv")
        .arg(format!("--input-ipc-server={}", mpv_socket))
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
    mpv.observe_property(MpvProps::PAUSE as isize, "pause")
        .unwrap();
    mpv.observe_property(MpvProps::PLAYBACKTIME as isize, "playback-time")
        .unwrap();
    mpv.observe_property(MpvProps::SEEKING as isize, "seeking")
        .unwrap();

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
            tokio::spawn(async move { handle_connection(mpv, addr, stream, state, true).await });
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

        let communication_task =
            tokio::spawn(async move { handle_connection(mpv, addr, stream, state, false).await });
        let _ = tokio::join!(communication_task);
    }
}

async fn handle_connection(
    mut mpv: Mpv,
    addr: SocketAddr,
    stream: TcpStream,
    state: Arc<Mutex<Shared>>,
    is_serving: bool,
) {
    println!("accepted connection");
    let (rx, mut tx) = stream.into_split();

    if !is_serving {
        // Wait for mpv to load a file
        while !matches!(mpv.event_listen().unwrap(), Event::FileLoaded) {}
        tx.write_all(b"newconn:banana\n").await.unwrap();
    }

    let mut reader = BufReader::new(rx);
    state.lock().await.peers.insert(addr, tx);
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
                let (key, value) = line
                    .strip_suffix("\n")
                    .unwrap_or("_:_")
                    .split_once(':')
                    .unwrap();
                println!("{} : {}", key, value);
                match (key, value) {
                    ("pause", "true") => {
                        mpv.pause().unwrap();
                    }
                    ("pause", "false") => {
                        mpv.set_property("pause", false).unwrap();
                    }
                    ("seek", t) => {
                        let current_time =
                            mpv.get_property_string("playback-time").unwrap_or_default();
                        if dbg!(t) != dbg!(current_time) {
                            mpv.seek(t.parse().unwrap_or_default(), SeekOptions::Absolute)
                                .unwrap()
                        }
                    }
                    ("newconn", username) => {
                        if !username.chars().all(char::is_alphanumeric) {
                            break;
                        }
                        mpv.pause().unwrap();
                        mpv.run_command_raw(
                            "show-text",
                            &vec![format!("{username}: connected").as_str(), "2000"],
                        )
                        .unwrap();
                        let filename: String = mpv.get_property("filename").unwrap_or_default();
                        let duration = mpv.get_property_string("duration").unwrap_or_default();
                        let pause = mpv.get_property_string("pause").unwrap_or_default();
                        let current_time: f64 =
                            mpv.get_property("playback-time").unwrap_or_default();
                        println!("filename:{filename}\nduration:{duration}\nseek:{current_time}\npause:{pause}\n");
                        state
                            .lock()
                            .await
                            .send(
                                addr,
                                &format!(
                                    "filename:{filename}\nduration:{duration}\nseek:{current_time}\npause:{pause}\n"
                                ),
                            )
                            .await
                    }
                    ("filename", f) => {
                        if f != mpv.get_property::<String>("filename").unwrap_or_default() {
                            mpv.run_command_raw(
                                "show-text",
                                &vec!["filename does not match with server's filename", "2000"],
                            )
                            .unwrap();
                        }
                    }
                    ("duration", t) => {
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
                    println!("pause:{}", p);
                    let cloned_state = Arc::clone(&state);
                    tokio::task::spawn(async move {
                        cloned_state
                            .lock()
                            .await
                            .broadcast(&format!("pause:{:?}\n", p))
                            .await
                    });
                }
                Property::Unknown { name, data } => match name.as_str() {
                    "seeking" => {
                        println!("seeking:{:?}", data);
                        match data {
                            MpvDataType::Bool(false) => {
                                let cloned_state = Arc::clone(&state);
                                let current_time: f64 =
                                    mpv.get_property("playback-time").unwrap_or_default();
                                println!("myseek: {current_time}");
                                tokio::task::spawn(async move {
                                    cloned_state
                                        .lock()
                                        .await
                                        .broadcast(&format!("seek:{:?}\n", current_time))
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
                Property::PlaybackTime(_t) => {} //println!("{:?}", t),
                Property::Duration(_) => todo!(),
                Property::Metadata(_) => todo!(),
                _ => todo!(),
            },
            _ => {}
        }
    }
}
