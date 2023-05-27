use mpvipc::*;
use std::process::exit;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

use crate::{proto::*, Shared};

pub fn handle_mpv_event(mut mpv: Mpv, state: Arc<Mutex<Shared>>, standalone: bool) {
    // setup necessary property observers
    mpv.observe_property(0, "pause").unwrap();
    mpv.observe_property(1, "seeking").unwrap();

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
                    if standalone {
                        handle.block_on(s.broadcast(VoyeursCommand::Ready(!p)));
                    } else {
                        s.is_ready = !p;
                        match s.is_ready {
                            false => {
                                handle.block_on(s.broadcast(VoyeursCommand::Ready(false)));
                            }
                            true => {
                                if s.peers.values().into_iter().any(|r| !r.ready) {
                                    mpv.run_command_raw(
                                        "show-text",
                                        &vec!["Somebody isn't ready", "2000"],
                                    )
                                    .unwrap();
                                    s.ignore_next = true;
                                    mpv.pause().unwrap();
                                }
                                handle.block_on(s.broadcast(VoyeursCommand::Ready(true)));
                            }
                        }
                    }
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
