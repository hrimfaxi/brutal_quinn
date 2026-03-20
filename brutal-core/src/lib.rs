use std::time::{Duration, Instant};

use tracing::trace;

const SLOT_COUNT: usize = 5;

/// Configuration for the Brutal congestion controller.
#[derive(Debug, Clone)]
pub struct BrutalConfigCore {
    pub default_bandwidth_bps: u64,
    pub initial_rtt: Duration,
    pub min_window: u64,
    pub cwnd_gain: f64,
    pub min_ack_rate: f64,
    pub min_sample_count: u64,
    pub enable_ack_rate_compensation: bool,
}

impl Default for BrutalConfigCore {
    fn default() -> Self {
        Self {
            default_bandwidth_bps: 1_000_000,
            initial_rtt: Duration::from_millis(100),
            min_window: 16 * 1024,
            cwnd_gain: 1.25,
            min_ack_rate: 0.8,
            min_sample_count: 50,
            enable_ack_rate_compensation: false,
        }
    }
}

impl BrutalConfigCore {
    pub fn new(default_bandwidth_bps: u64) -> Self {
        Self {
            default_bandwidth_bps,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct PktInfoSlot {
    timestamp_sec: u64,
    ack_count: u64,
    loss_count: u64,
}

#[derive(Debug, Clone)]
pub struct BrutalCore {
    pub config: BrutalConfigCore,
    pub base_time: Instant,

    pub mtu: u64,
    pub bytes_in_flight: u64,

    pub smoothed_rtt: Option<Duration>,
    pub bandwidth_hint_bps: Option<u64>,

    pub ack_rate: f64,
    slots: [PktInfoSlot; SLOT_COUNT],
    pub acked_packets_in_batch: u64,
    pub lost_packets_in_batch: u64,

    pub cwnd: u64,
}

impl BrutalCore {
    pub fn new(config: BrutalConfigCore, now: Instant, current_mtu: u16) -> Self {
        let mtu = current_mtu as u64;
        let mut me = Self {
            config,
            base_time: now,
            mtu,
            bytes_in_flight: 0,
            smoothed_rtt: None,
            bandwidth_hint_bps: None,
            ack_rate: 1.0,
            slots: [PktInfoSlot::default(); SLOT_COUNT],
            acked_packets_in_batch: 0,
            lost_packets_in_batch: 0,
            cwnd: 0,
        };
        me.cwnd = me.compute_cwnd();
        me
    }

    pub fn target_bps(&self) -> u64 {
        self.bandwidth_hint_bps
            .unwrap_or(self.config.default_bandwidth_bps)
    }

    pub fn current_rtt(&self) -> Duration {
        self.smoothed_rtt.unwrap_or(self.config.initial_rtt)
    }

    pub fn effective_ack_rate(&self) -> f64 {
        if self.config.enable_ack_rate_compensation {
            self.ack_rate.max(self.config.min_ack_rate)
        } else {
            1.0
        }
    }

    pub fn estimate_packets(&self, bytes: u64) -> u64 {
        if bytes == 0 {
            return 0;
        }
        let mtu = self.mtu.max(1);
        bytes.div_ceil(mtu)
    }

    pub fn now_sec(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.base_time).as_secs()
    }

    pub fn compute_cwnd(&self) -> u64 {
        let bps = self.target_bps() as f64;
        let rtt = self.current_rtt().as_secs_f64();
        let ack_rate = self.effective_ack_rate();

        let cwnd = (bps * rtt * self.config.cwnd_gain / ack_rate / 8.0) as u64;
        cwnd.max(self.config.min_window).max(self.mtu)
    }

    pub fn update_ack_rate(&mut self, now: Instant) {
        let ts = self.now_sec(now);
        let idx = (ts % SLOT_COUNT as u64) as usize;

        if self.slots[idx].timestamp_sec == ts {
            self.slots[idx].ack_count += self.acked_packets_in_batch;
            self.slots[idx].loss_count += self.lost_packets_in_batch;
        } else {
            self.slots[idx] = PktInfoSlot {
                timestamp_sec: ts,
                ack_count: self.acked_packets_in_batch,
                loss_count: self.lost_packets_in_batch,
            };
        }

        let min_ts = ts.saturating_sub(SLOT_COUNT as u64);

        let mut ack = 0u64;
        let mut loss = 0u64;
        for slot in &self.slots {
            if slot.timestamp_sec >= min_ts {
                ack += slot.ack_count;
                loss += slot.loss_count;
            }
        }

        let total = ack + loss;
        if total < self.config.min_sample_count {
            self.ack_rate = 1.0;
        } else {
            self.ack_rate = ack as f64 / total as f64;
        }
    }

    pub fn refresh_cwnd(&mut self) {
        self.cwnd = self.compute_cwnd();
    }

    pub fn update_smoothed_rtt(&mut self, rtt: Duration) {
        self.smoothed_rtt = Some(rtt);
    }

    pub fn initial_window(&self) -> u64 {
        self.compute_cwnd()
    }

    pub fn window_cached(&self) -> u64 {
        self.cwnd
    }

    pub fn window_recomputed(&self) -> u64 {
        self.compute_cwnd()
    }

    pub fn on_sent(&mut self, bytes: u64) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(bytes);
    }

    pub fn on_ack_bytes(&mut self, bytes: u64, rtt: Duration) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);
        self.acked_packets_in_batch += self.estimate_packets(bytes);
        self.update_smoothed_rtt(rtt);
    }

    pub fn on_loss_bytes(&mut self, lost_bytes: u64) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(lost_bytes);

        if lost_bytes > 0 {
            self.lost_packets_in_batch += self.estimate_packets(lost_bytes);
        }
    }

    pub fn on_end_acks(&mut self, now: Instant) {
        self.update_ack_rate(now);
        self.refresh_cwnd();

        trace!(
            "[brutal] end_acks: target_bps={}, rtt_ms={}, ack_rate={:.3}, effective_ack_rate={:.3}, cwnd_gain={}, cwnd={}, in_flight={}, acked_pkts_batch={}, lost_pkts_batch={}, ack_comp={}",
            self.target_bps(),
            self.current_rtt().as_millis(),
            self.ack_rate,
            self.effective_ack_rate(),
            self.config.cwnd_gain,
            self.cwnd,
            self.bytes_in_flight,
            self.acked_packets_in_batch,
            self.lost_packets_in_batch,
            self.config.enable_ack_rate_compensation,
        );

        self.acked_packets_in_batch = 0;
        self.lost_packets_in_batch = 0;
    }

    pub fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = new_mtu as u64;
        self.refresh_cwnd();

        trace!("[brutal] mtu updated: mtu={}, cwnd={}", self.mtu, self.cwnd);
    }

    pub fn set_peer_bandwidth_hint(&mut self, bps: Option<u64>) {
        self.bandwidth_hint_bps = bps;
        self.refresh_cwnd();

        trace!(
            "[brutal] effective bandwidth updated: effective_bps={}, cwnd={}",
            self.target_bps(),
            self.cwnd,
        );
    }
}
