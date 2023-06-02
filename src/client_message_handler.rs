use mpvipc::Mpv;
use mpvipc::*;
use std::{collections::VecDeque, net::SocketAddr, sync::Arc};
use tokio::{net::TcpStream, sync::Mutex};
use url::Url;

use crate::{
    proto::*,
    time::{get_timestamp, get_weighted_latency, MAX_QUEUE_LATENCY},
    Peer, Settings, Shared,
};

pub async fn handle_connection(
    mut mpv: Mpv,
    addr: SocketAddr,
    stream: TcpStream,
    state: Arc<Mutex<Shared>>,
    settings: Settings,
) {
    println!("accepted connection");
    let (rx, tx) = stream.into_split();
    let reader = PacketReader::new(rx);
    state.lock().await.peers.insert(
        addr,
        Peer {
            tx,
            username: Default::default(),
            ready: false,
            latency: VecDeque::with_capacity(MAX_QUEUE_LATENCY),
        },
    );

    if !settings.is_serving {
        if settings.accept_source {
            state
                .lock()
                .await
                .send(addr, VoyeursCommand::GetStreamName)
                .await
        } else {
            state
                .lock()
                .await
                .send(
                    addr,
                    VoyeursCommand::NewConnection(settings.username.to_string()),
                )
                .await
        }
    }

    loop {
        match reader.read_packet().await {
            Ok(packet) => {
                let mut s = state.lock().await;

                let t_delta = get_timestamp() - packet.timestamp;
                let latency_vec = &mut s.peers.get_mut(&addr).unwrap().latency;
                if latency_vec.len() == MAX_QUEUE_LATENCY {
                    latency_vec.pop_back();
                }
                latency_vec.push_front(t_delta);

                println!(
                    "Avg Latency : {}ms , Current Latency: {}",
                    get_weighted_latency(latency_vec),
                    t_delta
                );

                match packet.command {
                    VoyeursCommand::Ready(p) => {
                        if settings.standalone {
                            if mpv.get_property::<bool>("pause").unwrap() == p {
                                s.ignore_next = true;
                                mpv.set_property("pause", !p).unwrap();
                            }
                            if settings.is_serving {
                                s.broadcast(VoyeursCommand::Ready(p)).await;
                            }
                        } else {
                            s.peers.get_mut(&addr).unwrap().ready = p;
                            match p {
                                false => {
                                    if !mpv.get_property::<bool>("pause").unwrap() {
                                        s.ignore_next = true;
                                        mpv.set_property("pause", true).unwrap();
                                    }
                                    if settings.is_serving {
                                        s.broadcast_excluding(VoyeursCommand::Ready(false), addr)
                                            .await;
                                    }
                                }
                                true => {
                                    if dbg!(s.is_ready) && dbg!(s.peers.values().all(|r| r.ready)) {
                                        if mpv.get_property::<bool>("pause").unwrap() {
                                            s.ignore_next = true;
                                            mpv.set_property("pause", false).unwrap();
                                        }

                                        if settings.is_serving {
                                            s.broadcast(VoyeursCommand::Ready(true)).await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    VoyeursCommand::Seek(t) => {
                        let current_time: f64 =
                            mpv.get_property("playback-time").unwrap_or_default();
                        if t != current_time {
                            s.ignore_next = true;

                            // If the file isn't loaded yet, the seek will fail
                            while mpv.seek(t, SeekOptions::Absolute).is_err() {}

                            if settings.is_serving {
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
                            &[format!("{username}: connected").as_str(), "2000"],
                        )
                        .unwrap();

                        let filename = mpv.get_property("filename").unwrap_or_default();
                        let duration = mpv.get_property("duration").unwrap_or_default();
                        let pause: bool = mpv.get_property("pause").unwrap_or_default();
                        let current_time = mpv.get_property("playback-time").unwrap_or_default();
                        s.peers.get_mut(&addr).unwrap().username = username;
                        s.send(addr, VoyeursCommand::Filename(filename)).await;
                        s.send(addr, VoyeursCommand::Duration(duration)).await;
                        s.send(addr, VoyeursCommand::Seek(current_time)).await;
                        s.send(addr, VoyeursCommand::Ready(!pause)).await;
                    }
                    VoyeursCommand::GetStreamName => {
                        if settings.is_serving {
                            // Check if path is a valid URL
                            // TODO: the correct way to check this is by using stream-open-filename and parsing its data

                            let mut streamname =
                                mpv.get_property_string("path").unwrap_or_default();
                            if Url::parse(&streamname).is_err() {
                                streamname = "".to_owned();
                            }
                            s.send(addr, VoyeursCommand::StreamName(streamname)).await;
                        }
                    }
                    VoyeursCommand::StreamName(stream) => {
                        if settings.accept_source {
                            if stream.is_empty() {
                                println!("Server is not streaming from a valid url")
                            }
                            mpv.run_command(MpvCommand::LoadFile {
                                file: stream.to_string(),
                                option: PlaylistAddOptions::Replace,
                            })
                            .unwrap();
                            while !matches!(mpv.event_listen().unwrap(), Event::FileLoaded) {}
                            s.send(
                                addr,
                                VoyeursCommand::NewConnection(settings.username.to_string()),
                            )
                            .await
                        }
                    }
                    VoyeursCommand::Filename(f) => {
                        if f != mpv.get_property::<String>("filename").unwrap_or_default() {
                            mpv.run_command_raw(
                                "show-text",
                                &["filename does not match with server's filename", "2000"],
                            )
                            .unwrap();
                        }
                    }
                    VoyeursCommand::Duration(t) => {
                        if t != mpv.get_property::<f64>("duration").unwrap_or_default() {
                            mpv.run_command_raw(
                                "show-text",
                                &["duration does not match with server's duration", "2000"],
                            )
                            .unwrap();
                        }
                    }
                }
            }
            Err(_) => {
                let mut s = state.lock().await;
                let peer = s.peers.remove(&addr).unwrap();
                mpv.run_command_raw(
                    "show-text",
                    &[format!("{} : disconnected", peer.username).as_str(), "2000"],
                )
                .unwrap();
                peer.tx.forget();
                break;
            }
        };
    }
}
