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
A single continuous episode of sleep-onset observation backed by a streaming Wearable, containing at most one sleep onset and closing upon sustained disconnection, manual closure while disconnected or in `Signal Lost`, host suspend or power-off, or sustained post-sleep wakefulness.
_Avoid_: Night, run, recording, log

**Anomalous Session**:
A Monitoring Session flagged as physiologically uncharacteristic (such as illness or alcohol intake) so detection relies solely on within-session relative shifts and calibration updates are quarantined.
_Avoid_: Bad night, outlier, error session

**Grace Period**:
The cancellable countdown window between sleep-onset confirmation and executing the configured Sleep Action Pipeline, during which desktop activity, explicit cancellation, or physiological arousal can abort execution.
_Avoid_: Countdown, delay, warning window, buffer

**Activity Hold**:
A fixed brief decision window entered when desktop activity occurs during a `Grace Period`, pausing the `Sleep Action Pipeline` countdown so the user can choose `Snooze` or `Cancel` before automatic cancellation.
_Avoid_: Decision window, grace pause, idle delay

**Sleep Action Pipeline**:
The ordered sequence of desktop and system actions (such as pausing media, running a custom hook, locking the screen, or suspending the host) executed when a Grace Period expires uncancelled.
_Avoid_: Trigger, automation, macro, action list

**Snooze**:
A temporary user-initiated suppression of sleep triggers during a Grace Period that pauses automation without recording a false-positive calibration penalty.
_Avoid_: Postpone, mute, cancel (reserve cancellation for waking false positives)

**Active Hours**:
An optional recurring local-clock window during which confirmed sleep onset arms a `Grace Period` and the `Sleep Action Pipeline`; when disabled, actions arm 24/7, while its configured end time still serves as the target for an `until-morning` pause.
_Avoid_: Schedule, quiet hours

**Pre-Candidate Baseline**:
The snapshot of the user's calm waking physiological reference (`Quiet-Wake Baseline`) captured immediately upon crossing the sleep-onset threshold into `Candidate` state, serving as the primary waking reference for relative deceleration and calibration recomputation.
_Avoid_: Onset baseline, pre-sleep average, candidate warmup

**Calibration Boundary**:
A reversible point-in-time marker in the calibration ledger that resets personal physiological calibration to `Cold Start` (folding only subsequent Monitoring Sessions) while preserving historical sessions and per-Wearable signal floors.
_Avoid_: Soft reset, reset epoch, baseline wipe, cutoff

**Ledger Bundle**:
A portable, schema-versioned export of a user's Monitoring Sessions, feedback verdicts, Calibration Boundaries, and optionally Epochs and enrolled Wearables used for cross-machine migration and deterministic calibration recomputation.
_Avoid_: Dump, snapshot, archive
