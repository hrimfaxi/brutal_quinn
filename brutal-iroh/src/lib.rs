use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use brutal_core::{BrutalConfigCore, BrutalCore};
use iroh_quinn_proto::congestion::{CongestionParameter, Controller, ControllerFactory};
use iroh_quinn_proto::{ConfigError, RttEstimator};
use tracing::trace;

#[derive(Debug, Clone, Default)]
pub struct BrutalConfig(pub BrutalConfigCore);

impl BrutalConfig {
    pub fn new(default_bandwidth_bps: u64) -> Self {
        Self(BrutalConfigCore::new(default_bandwidth_bps))
    }

    pub fn inner(&self) -> &BrutalConfigCore {
        &self.0
    }

    pub fn inner_mut(&mut self) -> &mut BrutalConfigCore {
        &mut self.0
    }
}

impl ControllerFactory for BrutalConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        let cfg = self.0.clone();

        trace!(
            "[brutal] build called: default_bandwidth_bps={}, initial_rtt_ms={}, min_window={}, cwnd_gain={}, min_ack_rate={}, min_sample_count={}, enable_ack_rate_compensation={}, mtu={}",
            cfg.default_bandwidth_bps,
            cfg.initial_rtt.as_millis(),
            cfg.min_window,
            cfg.cwnd_gain,
            cfg.min_ack_rate,
            cfg.min_sample_count,
            cfg.enable_ack_rate_compensation,
            current_mtu,
        );

        Box::new(Brutal(BrutalCore::new(cfg, now, current_mtu)))
    }
}

#[derive(Debug, Clone)]
pub struct Brutal(pub BrutalCore);

impl Controller for Brutal {
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn initial_window(&self) -> u64 {
        self.0.initial_window()
    }

    fn window(&self) -> u64 {
        self.0.window_recomputed()
    }

    fn on_sent(&mut self, _now: Instant, bytes: u64, _last_packet_number: u64) {
        self.0.on_sent(bytes);
    }

    fn on_ack(
        &mut self,
        _now: Instant,
        _sent: Instant,
        bytes: u64,
        _app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.0.on_ack_bytes(bytes, rtt.get());
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        _in_flight: u64,
        _app_limited: bool,
        _largest_packet_num_acked: Option<u64>,
    ) {
        self.0.on_end_acks(now);
    }

    fn on_congestion_event(
        &mut self,
        _now: Instant,
        _sent: Instant,
        _is_persistent_congestion: bool,
        _is_ecn: bool,
        lost_bytes: u64,
    ) {
        self.0.on_loss_bytes(lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.0.on_mtu_update(new_mtu);
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn set_parameter(&mut self, param: CongestionParameter) -> Result<(), ConfigError> {
        match param {
            CongestionParameter::PeerBandwidthHint(bps) => {
                self.0.set_peer_bandwidth_hint(Some(bps));
                self.0.refresh_cwnd();
            }

            CongestionParameter::CwndGain(gain) => {
                if gain <= 0.0 {
                    return Err(ConfigError::OutOfBounds);
                }
                self.0.config.cwnd_gain = gain;
                self.0.refresh_cwnd();
            }

            CongestionParameter::AckCompensation(enable) => {
                self.0.config.enable_ack_rate_compensation = enable;
            }
        }
        Ok(())
    }
}
