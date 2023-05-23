use std::str::FromStr;

use strum_macros::{Display, EnumString, IntoStaticStr};

const _PROTOCOL_VERSION: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Display, EnumString, IntoStaticStr)]
pub enum VoyeursCommand {
    #[strum(serialize = "newconn")]
    NewConnection,
    #[strum(serialize = "pause")]
    Pause,
    #[strum(serialize = "seek")]
    Seek,
    #[strum(serialize = "filename")]
    Filename,
    #[strum(serialize = "duration")]
    Duration,
    #[strum(serialize = "stream")]
    StreamName,
    Unknown,
}

impl Default for VoyeursCommand {
    fn default() -> Self {
        VoyeursCommand::Unknown
    }
}

pub fn line_to_kv(line: &str) -> (VoyeursCommand, &str) {
    println!("{}", line);
    let (key, value) = line
        .strip_suffix("\n")
        .unwrap_or("_:_")
        .split_once(':')
        .unwrap();
    (VoyeursCommand::from_str(key).unwrap_or_default(), value)
}

//TODO: write tests
