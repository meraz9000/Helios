use std::time::{Duration, Instant};

use helios_proto::{QualityMode, QualityTier};
use tokio::sync::Notify;

/// Network probe sample fed into `AdaptiveController` every ~500–1000ms.
#[derive(Debug, Clone, Copy)]
pub struct NetworkSample {
    pub rtt_ms: f32,
    pub packet_loss_pct: f32,
    pub measured_kbps: u32,
}

/// Congestion controller tuned specifically for interactive low-bandwidth remote desktop.
///
/// Design principle: **React fast downward (protect mouse/keyboard responsiveness),
/// recover slowly upward (avoid oscillating between video and survival tile modes).**
#[derive(Debug)]
pub struct AdaptiveController {
    mode: QualityMode,
    active_tier: QualityTier,
    baseline_rtt_ms: Option<f32>,
    last_upgrade: Instant,
    upgrade_cooldown: Duration,
    consecutive_good_samples: u32,
}

impl AdaptiveController {
    pub fn new(initial_mode: QualityMode) -> Self {
        let initial_tier = match initial_mode {
            QualityMode::SurvivalText => QualityTier::SurvivalText,
            QualityMode::LowBandwidth => QualityTier::SlowLink,
            QualityMode::Balanced | QualityMode::Auto => QualityTier::Constrained,
            QualityMode::Smooth => QualityTier::Normal,
        };
        Self {
            mode: initial_mode,
            active_tier: initial_tier,
            baseline_rtt_ms: None,
            last_upgrade: Instant::now(),
            upgrade_cooldown: Duration::from_secs(5),
            consecutive_good_samples: 0,
        }
    }

    pub fn set_mode(&mut self, mode: QualityMode) -> QualityTier {
        self.mode = mode;
        self.active_tier = match mode {
            QualityMode::Auto => self.active_tier,
            QualityMode::SurvivalText => QualityTier::SurvivalText,
            QualityMode::LowBandwidth => QualityTier::SlowLink,
            QualityMode::Balanced => QualityTier::Constrained,
            QualityMode::Smooth => QualityTier::Normal,
        };
        self.active_tier
    }

    pub fn mode(&self) -> QualityMode {
        self.mode
    }

    pub fn active_tier(&self) -> QualityTier {
        self.active_tier
    }

    /// Update controller with a new network measurement and return the active `QualityTier`.
    pub fn on_sample(&mut self, sample: NetworkSample) -> QualityTier {
        // Track minimum/baseline RTT to detect bufferbloat (queuing delay inflation)
        let baseline = match self.baseline_rtt_ms {
            Some(prev) => prev.min(sample.rtt_ms).max(1.0),
            None => sample.rtt_ms.max(1.0),
        };
        self.baseline_rtt_ms = Some(baseline);

        if self.mode != QualityMode::Auto {
            return self.active_tier;
        }

        let rtt_inflation_ms = (sample.rtt_ms - baseline).max(0.0);

        // Severe degradation: drop immediately to Survival / Text Mode
        if sample.packet_loss_pct >= 5.0
            || sample.measured_kbps < 150
            || rtt_inflation_ms > 220.0
        {
            self.consecutive_good_samples = 0;
            self.active_tier = QualityTier::SurvivalText;
            return self.active_tier;
        }

        // Moderate degradation: step down quickly
        let desired = if sample.packet_loss_pct >= 2.0
            || sample.measured_kbps < 500
            || rtt_inflation_ms > 100.0
        {
            QualityTier::SlowLink
        } else if sample.measured_kbps < 3_000 || rtt_inflation_ms > 45.0 {
            QualityTier::Constrained
        } else {
            QualityTier::Normal
        };

        if desired < self.active_tier {
            self.consecutive_good_samples = 0;
            self.active_tier = desired;
        } else if desired > self.active_tier {
            self.consecutive_good_samples += 1;
            if self.consecutive_good_samples >= 4
                && self.last_upgrade.elapsed() >= self.upgrade_cooldown
            {
                // Step up only one tier at a time for stability
                self.active_tier = match self.active_tier {
                    QualityTier::SurvivalText => QualityTier::SlowLink,
                    QualityTier::SlowLink => QualityTier::Constrained,
                    QualityTier::Constrained => QualityTier::Normal,
                    QualityTier::Normal => QualityTier::Normal,
                };
                self.consecutive_good_samples = 0;
                self.last_upgrade = Instant::now();
            }
        } else {
            self.consecutive_good_samples = 0;
        }

        self.active_tier
    }
}

/// Strict 1-frame anti-bufferbloat slot.
///
/// If the encoder produces a newer frame before the network task finishes sending
/// the previous frame, the stale frame is overwritten immediately and counted as
/// dropped — guaranteeing zero latency buildup even on 100 kbps links.
pub struct LatestFrameSlot<T> {
    inner: std::sync::Mutex<SlotState<T>>,
    notify: Notify,
}

struct SlotState<T> {
    frame: Option<T>,
    dropped_count: u64,
}

impl<T> Default for LatestFrameSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> LatestFrameSlot<T> {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(SlotState {
                frame: None,
                dropped_count: 0,
            }),
            notify: Notify::new(),
        }
    }

    /// Push a fresh frame, dropping any unsent older frame instead of queuing.
    pub fn push(&self, frame: T) -> bool {
        let mut guard = self.inner.lock().unwrap();
        let dropped = guard.frame.replace(frame).is_some();
        if dropped {
            guard.dropped_count += 1;
        }
        drop(guard);
        self.notify.notify_one();
        dropped
    }

    /// Wait for the latest frame and take ownership of it.
    pub async fn take(&self) -> T {
        loop {
            if let Some(frame) = self.inner.lock().unwrap().frame.take() {
                return frame;
            }
            self.notify.notified().await;
        }
    }

    pub fn dropped_count(&self) -> u64 {
        self.inner.lock().unwrap().dropped_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_drops_fast_on_bad_network_and_honors_manual_override() {
        let mut ctrl = AdaptiveController::new(QualityMode::Auto);
        assert_eq!(ctrl.active_tier(), QualityTier::Constrained);

        // Good baseline sample
        ctrl.on_sample(NetworkSample {
            rtt_ms: 35.0,
            packet_loss_pct: 0.0,
            measured_kbps: 2000,
        });

        // Sudden awful hotel Wi-Fi (120 kbps, 6% loss) -> immediate SurvivalText
        let tier = ctrl.on_sample(NetworkSample {
            rtt_ms: 310.0,
            packet_loss_pct: 6.5,
            measured_kbps: 120,
        });
        assert_eq!(tier, QualityTier::SurvivalText);

        // Manual override stays pinned regardless of samples
        ctrl.set_mode(QualityMode::SurvivalText);
        let pinned = ctrl.on_sample(NetworkSample {
            rtt_ms: 15.0,
            packet_loss_pct: 0.0,
            measured_kbps: 50_000,
        });
        assert_eq!(pinned, QualityTier::SurvivalText);
    }

    #[tokio::test]
    async fn latest_frame_slot_drops_stale_frames() {
        let slot = LatestFrameSlot::new();
        assert!(!slot.push(1u32));
        assert!(slot.push(2u32));
        assert!(slot.push(3u32));
        assert_eq!(slot.dropped_count(), 2);
        assert_eq!(slot.take().await, 3u32);
    }
}
