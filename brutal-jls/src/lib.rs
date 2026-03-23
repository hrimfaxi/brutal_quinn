use std::any::Any;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use brutal_core::{BrutalConfigCore, BrutalCore};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use quinn_proto_jls::congestion::{Controller, ControllerFactory};
use quinn_proto_jls::{ConfigError, RttEstimator};
use tracing::trace;

pub use brutal_core::CongestionParameter;

#[derive(Debug, Clone, Default)]
pub struct BrutalConfig(pub BrutalConfigCore);

impl BrutalConfig {
    pub fn new(
        default_bandwidth_bps: u64,
        cwnd_gain: f64,
        enable_ack_rate_compensation: bool,
    ) -> Self {
        Self(BrutalConfigCore::new(
            default_bandwidth_bps,
            cwnd_gain,
            enable_ack_rate_compensation,
        ))
    }

    pub fn inner(&self) -> &BrutalConfigCore {
        &self.0
    }

    pub fn inner_mut(&mut self) -> &mut BrutalConfigCore {
        &mut self.0
    }
}

impl Brutal {
    fn apply_parameter(&mut self, param: CongestionParameter) -> Result<(), ConfigError> {
        match param {
            CongestionParameter::PeerBandwidthHint(bps) => {
                self.core.set_peer_bandwidth_hint(Some(bps));
            }

            CongestionParameter::CwndGain(gain) => {
                if gain <= 0.0 {
                    return Err(ConfigError::OutOfBounds);
                }
                self.core.config.cwnd_gain = gain;
                self.core.refresh_cwnd();
            }

            CongestionParameter::AckCompensation(enable) => {
                self.core.config.enable_ack_rate_compensation = enable;
                self.core.refresh_cwnd();
            }
        }

        trace!(
            "[brutal] parameter applied: remote={}, cwnd={}, target_bps={}, ack_comp={}, cwnd_gain={}",
            self.remote,
            self.core.cwnd,
            self.core.target_bps(),
            self.core.config.enable_ack_rate_compensation,
            self.core.config.cwnd_gain,
        );

        Ok(())
    }

    fn apply_pending_parameters(&mut self) -> Result<(), ConfigError> {
        if let Some(v) = self.control.take_peer_bandwidth_hint() {
            self.apply_parameter(CongestionParameter::PeerBandwidthHint(v))?;
        }

        if let Some(v) = self.control.take_cwnd_gain() {
            self.apply_parameter(CongestionParameter::CwndGain(v))?;
        }

        if let Some(v) = self.control.take_ack_compensation() {
            self.apply_parameter(CongestionParameter::AckCompensation(v))?;
        }

        Ok(())
    }
}

#[derive(Debug, Default)]
struct BrutalControl {
    peer_bandwidth_hint: Mutex<Option<u64>>,
    cwnd_gain: Mutex<Option<f64>>,
    ack_compensation: Mutex<Option<bool>>,
}

impl BrutalControl {
    fn new() -> Self {
        Self {
            peer_bandwidth_hint: Mutex::new(None),
            cwnd_gain: Mutex::new(None),
            ack_compensation: Mutex::new(None),
        }
    }

    fn take_peer_bandwidth_hint(&self) -> Option<u64> {
        self.peer_bandwidth_hint
            .lock()
            .expect("mutex poisoned")
            .take()
    }

    fn take_cwnd_gain(&self) -> Option<f64> {
        self.cwnd_gain.lock().expect("mutex poisoned").take()
    }

    fn take_ack_compensation(&self) -> Option<bool> {
        self.ack_compensation.lock().expect("mutex poisoned").take()
    }

    fn store_parameter(&self, param: CongestionParameter) {
        match param {
            CongestionParameter::PeerBandwidthHint(v) => {
                *self.peer_bandwidth_hint.lock().expect("mutex poisoned") = Some(v);
            }
            CongestionParameter::CwndGain(v) => {
                *self.cwnd_gain.lock().expect("mutex poisoned") = Some(v);
            }
            CongestionParameter::AckCompensation(v) => {
                *self.ack_compensation.lock().expect("mutex poisoned") = Some(v);
            }
        }
    }
}

static BRUTAL_REGISTRY: Lazy<DashMap<SocketAddr, Weak<BrutalControl>>> =
    Lazy::new(|| DashMap::new());

fn register_controller(remote: SocketAddr, control: &Arc<BrutalControl>) {
    BRUTAL_REGISTRY.insert(remote, Arc::downgrade(control));
}

pub fn brutal_set_parameter_by_remote(
    remote: SocketAddr,
    param: CongestionParameter,
) -> Result<(), ConfigError> {
    if let Some(entry) = BRUTAL_REGISTRY.get(&remote) {
        if let Some(control) = entry.value().upgrade() {
            control.store_parameter(param);
            return Ok(());
        }
    }

    BRUTAL_REGISTRY.remove(&remote);
    Err(ConfigError::OutOfBounds)
}

impl ControllerFactory for BrutalConfig {
    fn build(
        self: Arc<Self>,
        now: Instant,
        current_mtu: u16,
        remote: &std::net::SocketAddr,
    ) -> Box<dyn Controller> {
        let cfg = self.0.clone();

        trace!(
            "[brutal] build called: default_bandwidth_bps={}, initial_rtt_ms={}, min_window={}, cwnd_gain={}, min_ack_rate={}, min_sample_count={}, enable_ack_rate_compensation={}, mtu={}, remote={:?}",
            cfg.default_bandwidth_bps,
            cfg.initial_rtt.as_millis(),
            cfg.min_window,
            cfg.cwnd_gain,
            cfg.min_ack_rate,
            cfg.min_sample_count,
            cfg.enable_ack_rate_compensation,
            current_mtu,
            *remote,
        );

        let control = Arc::new(BrutalControl::new());
        register_controller(*remote, &control);

        Box::new(Brutal {
            core: BrutalCore::new(cfg, now, current_mtu),
            remote: *remote,
            control,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Brutal {
    core: BrutalCore,
    remote: SocketAddr,
    control: Arc<BrutalControl>,
}

impl Drop for Brutal {
    fn drop(&mut self) {
        BRUTAL_REGISTRY.remove(&self.remote);
    }
}

impl Controller for Brutal {
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn initial_window(&self) -> u64 {
        self.core.initial_window()
    }

    fn window(&self) -> u64 {
        self.core.window_cached()
    }

    fn on_sent(&mut self, _now: Instant, bytes: u64, _last_packet_number: u64) {
        let _ = self.apply_pending_parameters();
        self.core.on_sent(bytes);
    }

    fn on_ack(
        &mut self,
        _now: Instant,
        _sent: Instant,
        bytes: u64,
        _app_limited: bool,
        rtt: &RttEstimator,
    ) {
        let _ = self.apply_pending_parameters();
        self.core.on_ack_bytes(bytes, rtt.get());
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        _in_flight: u64,
        _app_limited: bool,
        _largest_packet_num_acked: Option<u64>,
    ) {
        let _ = self.apply_pending_parameters();
        self.core.on_end_acks(now);
    }

    fn on_congestion_event(
        &mut self,
        _now: Instant,
        _sent: Instant,
        _is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        let _ = self.apply_pending_parameters();
        self.core.on_loss_bytes(lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        let _ = self.apply_pending_parameters();
        self.core.on_mtu_update(new_mtu);
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }
}
