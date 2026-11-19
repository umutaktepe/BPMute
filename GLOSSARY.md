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
