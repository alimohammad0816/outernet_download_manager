use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};

pub const MAX_SPEED_KIB: u32 = 1024 * 1024;

pub struct TransferControl {
    paused: AtomicBool,
    received: AtomicU64,
    completed: AtomicU64,
    total: Mutex<Option<u64>>,
    bytes_per_second: Option<u64>,
    next_chunk: Mutex<Instant>,
    samples: Mutex<VecDeque<(Instant, u64)>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    pub total_bytes: Option<u64>,
    pub completed_bytes: u64,
    pub bytes_per_second: u64,
}

impl TransferControl {
    pub fn new(limit_kib: Option<u32>) -> Self {
        let now = Instant::now();
        Self {
            paused: AtomicBool::new(false),
            received: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            total: Mutex::new(None),
            bytes_per_second: limit_kib.map(|rate| u64::from(rate) * 1024),
            next_chunk: Mutex::new(now),
            samples: Mutex::new(VecDeque::from([(now, 0)])),
        }
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn check(&self) -> Result<(), String> {
        if self.is_paused() {
            Err("Download paused.".into())
        } else {
            Ok(())
        }
    }

    pub fn set_total(&self, total: Option<u64>) {
        let mut current = self.total.lock().expect("transfer total lock");
        self.completed.store(0, Ordering::Relaxed);
        *current = total;
    }

    pub fn chunk_bytes(&self, maximum: usize) -> usize {
        self.bytes_per_second
            .map_or(maximum, |rate| maximum.min((rate / 8).max(1) as usize))
    }

    pub fn account_network(&self, bytes: usize) -> Result<(), String> {
        self.account_network_while(bytes, || true)
    }

    pub fn account_network_while(
        &self,
        bytes: usize,
        active: impl Fn() -> bool,
    ) -> Result<(), String> {
        let check = || -> Result<(), String> {
            self.check()?;
            if active() {
                Ok(())
            } else {
                Err("The current request was stopped.".into())
            }
        };
        check()?;
        if let Some(rate) = self.bytes_per_second {
            let deadline = {
                let mut next = self.next_chunk.lock().expect("bandwidth limiter lock");
                let interval = Duration::from_secs_f64(bytes as f64 / rate as f64);
                *next = (*next).max(Instant::now()) + interval;
                *next
            };
            loop {
                check()?;
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining <= Duration::from_millis(1) {
                    break;
                }
                thread::sleep(remaining.min(Duration::from_millis(30)));
            }
        }
        self.received.fetch_add(bytes as u64, Ordering::Relaxed);
        Ok(())
    }

    pub fn commit_bytes(&self, bytes: u64) {
        self.completed.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn discard_bytes(&self, bytes: u64) {
        self.completed.fetch_sub(bytes, Ordering::Relaxed);
    }

    pub fn progress(&self) -> TransferProgress {
        let mut samples = self.samples.lock().expect("speed samples lock");
        let now = Instant::now();
        let received = self.received.load(Ordering::Relaxed);
        samples.push_back((now, received));
        while samples.len() > 2
            && (samples.len() > 16 || now.duration_since(samples[1].0) > Duration::from_secs(3))
        {
            samples.pop_front();
        }
        let (start, before) = samples[0];
        let seconds = now.duration_since(start).as_secs_f64();
        let bytes_per_second = if seconds >= 0.25 {
            (received.saturating_sub(before) as f64 / seconds).round() as u64
        } else {
            0
        };
        let bytes_per_second = self
            .bytes_per_second
            .map_or(bytes_per_second, |limit| bytes_per_second.min(limit));
        let total = self.total.lock().expect("transfer total lock");
        TransferProgress {
            total_bytes: *total,
            completed_bytes: self.completed.load(Ordering::Relaxed),
            bytes_per_second,
        }
    }
}
