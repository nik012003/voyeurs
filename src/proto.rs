use std::mem::size_of;
use std::time::{SystemTime, UNIX_EPOCH};
use std::vec;
use std::{error::Error, fmt};
use tokio::net::tcp::OwnedReadHalf;

const PROTOCOL_VERSION: u16 = 1;

// Packet structure
// ____________________________________________________
// |               |          |             |         |
// |   timestamp   | cmd_code |    lenght   | args ...|
// |_______________|__________|_____________|_________| ......
// ^               ^          ^             ^                 ^
// |    8 bytes    |  1 byte  |   2 bytes   |  $lenght bytes  |

type TsSize = u64;
type CmdSize = u8;
type LenSize = u16;

pub struct PacketReader {
    pub inner: OwnedReadHalf,
}

#[derive(Debug)]
struct UncompatibleProtocolVersion {
    ver: u16,
}
impl Error for UncompatibleProtocolVersion {}
impl fmt::Display for UncompatibleProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Protocol version {} is incompatible", self.ver)
    }
}
#[derive(Debug)]
struct UnkownCommand {
    cmd: u8,
}
impl Error for UnkownCommand {}
impl fmt::Display for UnkownCommand {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Command {} is unknown", self.cmd)
    }
}

#[derive(Debug)]
struct TooShort;
impl Error for TooShort {}
impl fmt::Display for TooShort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Args read were smaller than the advertised length")
    }
}

impl PacketReader {
    pub fn new(inner: OwnedReadHalf) -> Self {
        Self { inner }
    }

    pub async fn read_packet(&self) -> Result<Packet, Box<dyn Error + Sync + Send>> {
        self.inner.readable().await?;

        let mut timestamp_buf: [u8; size_of::<TsSize>()] = Default::default();
        // This loop is needed as readable() could return false-positives.
        // TODO: only catch WouldBlock
        while self.inner.try_read(&mut timestamp_buf).is_err() {
            self.inner.readable().await?;
        }
        let timestamp = TsSize::from_be_bytes(timestamp_buf);

        let mut command_buf: [u8; size_of::<CmdSize>()] = Default::default();
        self.inner.try_read(&mut command_buf)?;
        let cmd_code = CmdSize::from_be_bytes(command_buf);

        let mut len: [u8; size_of::<LenSize>()] = Default::default();
        self.inner.try_read(&mut len)?;
        let len = LenSize::from_be_bytes(len);

        let mut args: Vec<u8> = vec![0; len as usize];
        if len > 0 {
            if self.inner.try_read(&mut args)? != len as usize {
                return Err(Box::new(TooShort));
            }
        }

        let command = VoyeursCommand::from_bytes(cmd_code, args)?;

        Ok(Packet { timestamp, command })
    }
}

#[derive(Debug)]
pub struct Packet {
    pub timestamp: TsSize,
    pub command: VoyeursCommand,
}

impl Packet {
    pub fn compile(self) -> Vec<u8> {
        let mut payload: Vec<u8> = vec![];
        payload.append(self.timestamp.to_be_bytes().to_vec().as_mut());

        let (cmd_code, args) = self.command.to_bytes();

        payload.push(cmd_code);
        let mut len = (args.len() as LenSize).to_be_bytes().to_vec();
        payload.append(&mut len);
        payload.append(args.clone().as_mut());
        payload
    }
}

#[derive(Debug, Clone, PartialEq)]

pub enum VoyeursCommand {
    NewConnection(String), // 0x00
    Ready(bool),           // 0x01
    Seek(f64),             // 0x02
    Filename(String),      // 0x03
    Duration(f64),         // 0x04
    StreamName(String),    // 0x05
    GetStreamName,         // 0x06
}

impl VoyeursCommand {
    fn to_bytes(&self) -> (CmdSize, Vec<u8>) {
        let cmd_code;
        let mut args;
        match self {
            VoyeursCommand::NewConnection(name) => {
                cmd_code = 0x00;
                args = vec![];
                args.append(&mut PROTOCOL_VERSION.to_be_bytes().to_vec());
                args.append(&mut name.as_bytes().to_vec());
            }
            VoyeursCommand::Ready(p) => {
                cmd_code = 0x01;
                args = [p.clone() as u8].to_vec();
            }
            VoyeursCommand::Seek(t) => {
                cmd_code = 0x02;
                args = t.to_be_bytes().to_vec();
            }
            VoyeursCommand::Filename(f) => {
                cmd_code = 0x03;
                args = f.as_bytes().to_vec();
            }
            VoyeursCommand::Duration(t) => {
                cmd_code = 0x04;
                args = t.to_be_bytes().to_vec();
            }
            VoyeursCommand::StreamName(n) => {
                cmd_code = 0x05;
                args = n.as_bytes().to_vec();
            }
            VoyeursCommand::GetStreamName => {
                cmd_code = 0x06;
                args = vec![];
            }
        }
        (cmd_code, args)
    }

    pub fn from_bytes(
        cmd_code: CmdSize,
        args: Vec<u8>,
    ) -> Result<Self, Box<dyn Error + Sync + Send>> {
        match cmd_code {
            0x00 => {
                // The first 2 bytes of the vector reperent the protocol version
                let ver = u16::from_be_bytes(args.get(0..2).ok_or(TooShort)?.try_into()?);
                if ver != PROTOCOL_VERSION {
                    return Err(Box::new(UncompatibleProtocolVersion { ver }));
                }
                Ok(VoyeursCommand::NewConnection(String::from_utf8(
                    args[2..].to_vec(),
                )?))
            }
            0x01 => Ok(VoyeursCommand::Ready(*args.get(0).unwrap() == 1)),
            0x02 => {
                let time: f64 = f64::from_be_bytes(args.get(0..8).ok_or(TooShort)?.try_into()?);
                Ok(VoyeursCommand::Seek(time))
            }
            0x03 => Ok(VoyeursCommand::Filename(String::from_utf8(args)?)),
            0x04 => {
                let time: f64 = f64::from_be_bytes(args.get(0..8).ok_or(TooShort)?.try_into()?);
                Ok(VoyeursCommand::Duration(time))
            }
            0x05 => Ok(VoyeursCommand::StreamName(String::from_utf8(args)?)),
            0x06 => Ok(VoyeursCommand::GetStreamName),
            cmd => return Err(Box::new(UnkownCommand { cmd })),
        }
    }

    pub fn craft_packet(self) -> Packet {
        let timestamp: TsSize = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as TsSize;
        Packet {
            timestamp,
            command: self,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::proto::VoyeursCommand;

    #[test]
    fn test_command_parser() {
        check_parse(VoyeursCommand::NewConnection("test".to_string()));
        check_parse(VoyeursCommand::Ready(false));
        check_parse(VoyeursCommand::Seek(0.0));
        check_parse(VoyeursCommand::Filename("test".to_string()));
        check_parse(VoyeursCommand::Duration(1.0));
        check_parse(VoyeursCommand::StreamName("test".to_string()));
        check_parse(VoyeursCommand::GetStreamName);
    }

    fn check_parse(cmd: VoyeursCommand) {
        let (cmd_code, args) = cmd.to_bytes();
        assert_eq!(cmd, VoyeursCommand::from_bytes(cmd_code, args).unwrap());
    }
}
