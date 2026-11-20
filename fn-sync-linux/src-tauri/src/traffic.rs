//! 全局流量计数（get_traffic_query: tx/rx，官方 bar 页速率显示语义）。

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

static TX: AtomicI64 = AtomicI64::new(0);
static RX: AtomicI64 = AtomicI64::new(0);
static SAMPLE: LazyLock<Mutex<(Instant, i64, i64)>> = LazyLock::new(|| {
    Mutex::new((
        Instant::now(),
        TX.load(Ordering::Relaxed),
        RX.load(Ordering::Relaxed),
    ))
});

#[derive(Default)]
pub struct TrafficCounter;

impl TrafficCounter {
    pub fn new() -> Self {
        Self
    }

    pub fn load(&self) -> (i64, i64) {
        let tx = TX.load(Ordering::Relaxed);
        let rx = RX.load(Ordering::Relaxed);
        let mut sample = SAMPLE.lock().unwrap_or_else(|e| e.into_inner());
        let seconds = sample.0.elapsed().as_secs_f64().max(0.001);
        let rates = (
            ((tx - sample.1) as f64 / seconds) as i64,
            ((rx - sample.2) as f64 / seconds) as i64,
        );
        *sample = (Instant::now(), tx, rx);
        rates
    }

    pub fn add(&self, tx: i64, rx: i64) {
        TX.fetch_add(tx, Ordering::Relaxed);
        RX.fetch_add(rx, Ordering::Relaxed);
    }
}

use std::sync::atomic::AtomicU64;
static UP_LIMIT: AtomicU64 = AtomicU64::new(0);
static DOWN_LIMIT: AtomicU64 = AtomicU64::new(0);
static UP_NEXT: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));
static DOWN_NEXT: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

pub fn set_limit(key: &str, value: &str) {
    let bytes = value
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .map(|n| (n as u64).saturating_mul(1024))
        .unwrap_or(0);
    let (limit, next) = match key {
        "global_upload_speed_limit" => (&UP_LIMIT, &UP_NEXT),
        "global_download_speed_limit" => (&DOWN_LIMIT, &DOWN_NEXT),
        _ => return,
    };
    limit.store(bytes, Ordering::Relaxed);
    *next.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
}

pub async fn throttle(bytes: usize, upload: bool) {
    let (limit, next) = if upload {
        (&UP_LIMIT, &UP_NEXT)
    } else {
        (&DOWN_LIMIT, &DOWN_NEXT)
    };
    let rate = limit.load(Ordering::Relaxed);
    if rate == 0 {
        return;
    }
    let until = {
        let mut next = next.lock().unwrap_or_else(|e| e.into_inner());
        let start = (*next).max(Instant::now());
        *next = start + std::time::Duration::from_secs_f64(bytes as f64 / rate as f64);
        *next
    };
    tokio::time::sleep_until(tokio::time::Instant::from_std(until)).await;
}
