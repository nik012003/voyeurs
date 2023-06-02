use lazy_static::lazy_static;
use rsntp::SntpClient;
use std::collections::VecDeque;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::proto::TsSize;

lazy_static! {
    pub static ref TIME_DELTA: Arc<AtomicI64> = Arc::new(AtomicI64::new(0));
}

pub fn get_timestamp() -> TsSize {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Couldn't get system time")
        .as_millis() as TsSize
        + TIME_DELTA.load(std::sync::atomic::Ordering::SeqCst) as TsSize
}

pub fn set_time_delta(ntp_server: String) {
    let client = SntpClient::new();
    let result = client
        .synchronize(ntp_server)
        .expect("Coudn't syncronize time with ntp server");
    let delta: i64 = result.datetime().unix_timestamp().expect("msg").as_millis() as i64
        - SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Couldn't get system time")
            .as_millis() as i64;
    println!("Clock skew: {} ms", delta);
    TIME_DELTA.store(delta, std::sync::atomic::Ordering::SeqCst);
}

pub static MAX_QUEUE_LATENCY: usize = 10;

pub fn get_weighted_latency(latency: &VecDeque<u64>) -> u64 {
    latency
        .iter()
        .enumerate()
        .map(|(i, l)| l * (MAX_QUEUE_LATENCY as u64 / (i + 1) as u64))
        .sum::<u64>()
        / latency.len() as u64
}
