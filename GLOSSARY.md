# BPMute

Linux background sleep-onset monitor and desktop automation daemon driven by standard Bluetooth heart-rate wearables.

## Language

**Wearable**:
A user's Bluetooth heart-rate hardware (smartwatch, chest strap, or optical armband) capable of broadcasting heart-rate measurements to BPMute.
_Avoid_: Device, watch, tracker, sensor

**Enrollment**:
The act of registering a Wearable with BPMute so the daemon recognizes it, persists it for automatic reconnection, and accepts its heart-rate stream.
_Avoid_: Pairing (reserve pairing/bonding strictly for Bluetooth security key exchange)

**Signal Lost**:
A state in which an active Wearable remains connected but stops delivering valid heart-rate readings (for example, when removed from the wrist or during an optical lock loss).
_Avoid_: Disconnected, offline

**Epoch**:
A fixed-interval aggregated snapshot of filtered heart-rate features and sleep-state probabilities computed from a Wearable's stream.
_Avoid_: Frame, tick, sample, window

**Quiet-Wake Baseline**:
The user's calm waking physiological reference within a session, used to measure relative heart-rate deceleration and variability collapse toward sleep onset.
_Avoid_: Resting baseline, initial baseline, warmup average

**Calibration Maturity**:
The readiness tier (`Cold Start`, `Learning`, or `Calibrated`) indicating how strongly personal session history anchors sleep-onset detection relative to population priors.
_Avoid_: Calibration level, training status, model confidence

**Monitoring Session**:
A single continuous episode of sleep-onset observation backed by a streaming Wearable, containing at most one sleep onset and closing upon sustained disconnection, host suspend or power-off, or sustained post-sleep wakefulness.
_Avoid_: Night, run, recording, log

**Anomalous Session**:
A Monitoring Session flagged as physiologically uncharacteristic (such as illness or alcohol intake) so detection relies solely on within-session relative shifts and calibration updates are quarantined.
_Avoid_: Bad night, outlier, error session
