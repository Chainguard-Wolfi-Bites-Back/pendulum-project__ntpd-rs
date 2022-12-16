use std::{collections::HashMap, fmt::Debug, hash::Hash};

use tracing::{error, info, instrument};

use crate::{
    Measurement, NtpClock, NtpDuration, NtpLeapIndicator, NtpPacket, NtpTimestamp,
    ObservablePeerTimedata, StateUpdate, SystemConfig, TimeSnapshot, TimeSyncController,
};

use self::{
    config::AlgorithmConfig,
    matrix::{Matrix, Vector},
    peer::PeerState,
};

mod config;
mod matrix;
mod peer;
mod select;

fn sqr(x: f64) -> f64 {
    x * x
}

#[derive(Debug, Clone)]
struct PeerSnapshot<Index: Copy> {
    index: Index,
    state: Vector,
    uncertainty: Matrix,
    delay: f64,

    peer_uncertainty: NtpDuration,
    peer_delay: NtpDuration,
    leap_indicator: NtpLeapIndicator,

    last_update: NtpTimestamp,
}

impl<Index: Copy> PeerSnapshot<Index> {
    fn offset(&self) -> f64 {
        self.state.entry(0)
    }

    fn offset_uncertainty(&self) -> f64 {
        self.uncertainty.entry(0, 0).sqrt()
    }

    fn observe(&self) -> ObservablePeerTimedata {
        ObservablePeerTimedata {
            offset: NtpDuration::from_seconds(self.offset()),
            uncertainty: NtpDuration::from_seconds(self.offset_uncertainty()),
            delay: NtpDuration::from_seconds(self.delay),
            remote_delay: self.peer_delay,
            remote_uncertainty: self.peer_uncertainty,
            last_update: self.last_update,
        }
    }
}

struct Combine<Index: Copy> {
    estimate: Vector,
    uncertainty: Matrix,
    peers: Vec<Index>,
    delay: NtpDuration,
    leap_indicator: Option<NtpLeapIndicator>,
}

fn vote_leap<Index: Copy>(selection: &[PeerSnapshot<Index>]) -> Option<NtpLeapIndicator> {
    let mut votes_59 = 0;
    let mut votes_61 = 0;
    let mut votes_none = 0;
    for snapshot in selection {
        match snapshot.leap_indicator {
            NtpLeapIndicator::NoWarning => votes_none += 1,
            NtpLeapIndicator::Leap61 => votes_61 += 1,
            NtpLeapIndicator::Leap59 => votes_59 += 1,
            NtpLeapIndicator::Unknown => {
                panic!("Unsynchronized peer selected for synchronization!")
            }
        }
    }
    if votes_none * 2 > selection.len() {
        Some(NtpLeapIndicator::NoWarning)
    } else if votes_59 * 2 > selection.len() {
        Some(NtpLeapIndicator::Leap59)
    } else if votes_61 * 2 > selection.len() {
        Some(NtpLeapIndicator::Leap61)
    } else {
        None
    }
}

fn combine<Index: Copy>(selection: &[PeerSnapshot<Index>]) -> Option<Combine<Index>> {
    if let Some(first) = selection.first() {
        let mut estimate = first.state;
        let mut uncertainty =
            first.uncertainty + Matrix::new(sqr(first.peer_uncertainty.to_seconds()), 0., 0., 0.);

        let mut used_peers = vec![(first.index, uncertainty.determinant())];

        for snapshot in selection.iter().skip(1) {
            let peer_estimate = snapshot.state;
            let peer_uncertainty = snapshot.uncertainty
                + Matrix::new(sqr(snapshot.peer_uncertainty.to_seconds()), 0., 0., 0.);

            used_peers.push((snapshot.index, peer_uncertainty.determinant()));

            // Merge measurements
            let mixer = (uncertainty + peer_uncertainty).inverse();
            estimate = estimate + uncertainty * mixer * (peer_estimate - estimate);
            uncertainty = uncertainty * mixer * peer_uncertainty;
        }

        used_peers.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        Some(Combine {
            estimate,
            uncertainty,
            peers: used_peers.iter().map(|v| v.0).collect(),
            delay: selection
                .iter()
                .map(|v| NtpDuration::from_seconds(v.delay) + v.peer_delay)
                .min()
                .unwrap_or(NtpDuration::from_seconds(first.delay) + first.peer_delay),
            leap_indicator: vote_leap(selection),
        })
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct KalmanClockController<C: NtpClock, PeerID: Hash + Eq + Copy + Debug> {
    peers: HashMap<PeerID, (PeerState, bool)>,
    clock: C,
    config: SystemConfig,
    algo_config: AlgorithmConfig,
    ignore_before: NtpTimestamp,
    freq_offset: f64,
    timedata: TimeSnapshot,
    desired_freq: f64,
    in_startup: bool,
}

impl<C: NtpClock, PeerID: Hash + Eq + Copy + Debug> KalmanClockController<C, PeerID> {
    #[instrument(skip(self))]
    fn update_peer(
        &mut self,
        id: PeerID,
        measurement: Measurement,
        packet: NtpPacket<'static>,
    ) -> bool {
        if measurement.localtime - self.ignore_before < NtpDuration::ZERO {
            return false;
        }

        self.peers.get_mut(&id).map(|state| {
            state
                .0
                .update(&self.config, &self.algo_config, measurement, packet)
                & state.1
        }) == Some(true)
    }

    fn update_clock(&mut self, time: NtpTimestamp) -> StateUpdate<PeerID> {
        // ensure all filters represent the same (current) time
        if self
            .peers
            .iter()
            .filter_map(|(_, (state, _))| state.get_filtertime())
            .any(|peertime| time - peertime < NtpDuration::ZERO)
        {
            return StateUpdate {
                used_peers: None,
                timesnapshot: Some(self.timedata),
                next_update: None,
            };
        }
        for (_, (state, _)) in self.peers.iter_mut() {
            state.progress_filtertime(time);
        }

        let selection = select::select(
            &self.config,
            &self.algo_config,
            self.peers
                .iter()
                .filter_map(|(index, (state, usable))| {
                    if *usable {
                        state.snapshot(*index)
                    } else {
                        None
                    }
                })
                .collect(),
        );

        if let Some(combined) = combine(&selection) {
            info!(
                "Offset: {}+-{}ms, frequency: {}+-{}ppm",
                combined.estimate.entry(0) * 1e3,
                combined.uncertainty.entry(0, 0).sqrt() * 1e3,
                combined.estimate.entry(1) * 1e6,
                combined.uncertainty.entry(1, 1).sqrt() * 1e6
            );

            let freq_delta = combined.estimate.entry(1) - self.desired_freq;
            let freq_uncertainty = combined.uncertainty.entry(1, 1).sqrt();
            if freq_delta.abs() > freq_uncertainty * self.algo_config.steer_frequency_threshold {
                self.steer_frequency(
                    freq_delta
                        - freq_uncertainty
                            * self.algo_config.steer_frequency_leftover
                            * freq_delta.signum(),
                );
            }

            let offset_delta = combined.estimate.entry(0);
            let offset_uncertainty = combined.uncertainty.entry(0, 0).sqrt();
            let next_update = if self.desired_freq == 0.0
                && offset_delta.abs() > offset_uncertainty * self.algo_config.steer_offset_threshold
            {
                self.steer_offset(
                    offset_delta
                        - offset_uncertainty
                            * self.algo_config.steer_offset_leftover
                            * offset_delta.signum(),
                )
            } else {
                None
            };

            // Unwrap is ok since selection will always be non-empty
            self.timedata.root_delay = combined.delay;
            self.timedata.root_dispersion =
                NtpDuration::from_seconds(combined.uncertainty.entry(0, 0).sqrt());
            self.clock
                .error_estimate_update(self.timedata.root_dispersion, self.timedata.root_delay)
                .expect("Cannot update clock");

            if let Some(leap) = combined.leap_indicator {
                self.clock.status_update(leap).expect("Cannot update clock");
            }

            // After a succesfull measurement we are out of startup.
            self.in_startup = false;

            StateUpdate {
                used_peers: Some(combined.peers),
                timesnapshot: Some(self.timedata),
                next_update,
            }
        } else {
            info!("No concensus cluster found");
            StateUpdate {
                used_peers: None,
                timesnapshot: Some(self.timedata),
                next_update: None,
            }
        }
    }

    fn check_offset_steer(&mut self, change: f64) {
        let change = NtpDuration::from_seconds(change);
        if self.in_startup {
            if !self.config.startup_panic_threshold.is_within(change) {
                error!("Unusually large clock step suggested, please manually verify system clock and reference clock state and restart if appropriate.");
                std::process::exit(exitcode::SOFTWARE);
            }
        } else {
            self.timedata.accumulated_steps += change;
            if !self.config.panic_threshold.is_within(change)
                || !self
                    .config
                    .accumulated_threshold
                    .map(|v| change.abs() < v)
                    .unwrap_or(true)
            {
                error!("Unusually large clock step suggested, please manually verify system clock and reference clock state and restart if appropriate.");
                std::process::exit(exitcode::SOFTWARE);
            }
        }
    }

    fn steer_offset(&mut self, change: f64) -> Option<NtpTimestamp> {
        self.check_offset_steer(change);
        if change.abs() > self.algo_config.jump_threshold {
            // jump
            self.clock
                .step_clock(NtpDuration::from_seconds(change))
                .unwrap();
            for (state, _) in self.peers.values_mut() {
                state.process_offset_steering(change)
            }
            info!("Jumped offset by {}ms", change * 1e3);
            None
        } else {
            // start slew
            let freq = self
                .algo_config
                .slew_max_frequency_offset
                .min(change.abs() / self.algo_config.slew_min_duration);
            self.desired_freq = -freq * change.signum();
            let duration = NtpDuration::from_seconds(change.abs() / freq);
            info!(
                "Slewing by {}ms over {}s",
                change * 1e3,
                duration.to_seconds()
            );
            Some(self.steer_frequency(-self.desired_freq) + duration)
        }
    }

    fn steer_frequency(&mut self, change: f64) -> NtpTimestamp {
        self.freq_offset = (1.0 + self.freq_offset) * (1.0 + change) - 1.0;
        self.clock.set_frequency(self.freq_offset).unwrap();
        let freq_update = self.clock.now().unwrap();
        for (state, _) in self.peers.values_mut() {
            state.process_frequency_steering(freq_update, change)
        }
        info!(
            "Changed frequency, current steer {}ppm, desired freq {}ppm",
            self.freq_offset * 1e6,
            self.desired_freq * 1e6,
        );
        freq_update
    }

    fn update_desired_poll(&mut self) {
        self.timedata.poll_interval = self
            .peers
            .values()
            .map(|(state, _)| state.get_desired_poll(&self.config.poll_limits))
            .min()
            .unwrap_or(self.config.poll_limits.max);
    }
}

impl<C: NtpClock, PeerID: Hash + Eq + Copy + Debug> TimeSyncController<C, PeerID>
    for KalmanClockController<C, PeerID>
{
    type AlgorithmConfig = AlgorithmConfig;

    fn new(clock: C, config: SystemConfig, algo_config: Self::AlgorithmConfig) -> Self {
        // Setup clock
        clock
            .disable_ntp_algorithm()
            .expect("Unable to change system time");
        clock
            .status_update(NtpLeapIndicator::Unknown)
            .expect("Unable to update clock");
        clock
            .set_frequency(0.0)
            .expect("Unable to set system clock frequency");

        KalmanClockController {
            peers: HashMap::new(),
            ignore_before: clock.now().unwrap(),
            clock,
            config,
            algo_config,
            freq_offset: 0.0,
            desired_freq: 0.0,
            timedata: TimeSnapshot::default(),
            in_startup: false,
        }
    }

    fn update_config(&mut self, config: SystemConfig, algo_config: Self::AlgorithmConfig) {
        self.config = config;
        self.algo_config = algo_config;
    }

    fn peer_add(&mut self, id: PeerID) {
        self.peers.insert(id, (PeerState::new(), false));
    }

    fn peer_remove(&mut self, id: PeerID) {
        self.peers.remove(&id);
    }

    fn peer_update(&mut self, id: PeerID, usable: bool) {
        if let Some(state) = self.peers.get_mut(&id) {
            state.1 = usable;
        }
    }

    fn peer_measurement(
        &mut self,
        id: PeerID,
        measurement: Measurement,
        packet: NtpPacket<'static>,
    ) -> StateUpdate<PeerID> {
        let should_update_clock = self.update_peer(id, measurement, packet);
        self.update_desired_poll();
        if should_update_clock {
            self.update_clock(measurement.localtime)
        } else {
            StateUpdate {
                used_peers: None,
                timesnapshot: Some(self.timedata),
                next_update: None,
            }
        }
    }

    fn time_update(&mut self) -> StateUpdate<PeerID> {
        // End slew
        self.steer_frequency(self.desired_freq);
        self.desired_freq = 0.0;
        StateUpdate::default()
    }

    fn peer_snapshot(&self, id: PeerID) -> Option<ObservablePeerTimedata> {
        self.peers
            .get(&id)
            .and_then(|v| v.0.snapshot(id))
            .map(|v| v.observe())
    }
}