//! Temporary enrollment-scoped BlueZ pairing agent bridging PIN/Passkey over IPC (ADR-0003).

use tokio::sync::{mpsc, oneshot};
use zbus::fdo;
use zbus::interface;
use zbus::zvariant::OwnedObjectPath;

/// Canonical D-Bus object path for BPMute's temporary enrollment agent.
pub const ENROLLMENT_AGENT_PATH: &str = "/org/bpmute/agent";

/// Authentication request bridging BlueZ pairing callbacks to the enrolling client.
#[derive(Debug)]
pub enum AgentAuthRequest {
    /// 6-digit numeric comparison passkey confirmation prompt.
    Confirmation {
        device: String,
        passkey: u32,
        reply: oneshot::Sender<bool>,
    },
    /// Request for passkey entry from client.
    Passkey {
        device: String,
        reply: oneshot::Sender<Option<u32>>,
    },
    /// Request for PIN code entry from client.
    PinCode {
        device: String,
        reply: oneshot::Sender<Option<String>>,
    },
    /// General authorization request.
    Authorization {
        device: String,
        reply: oneshot::Sender<bool>,
    },
}

/// Server-side D-Bus object implementing `org.bluez.Agent1`.
#[derive(Clone)]
pub struct EnrollmentAgent {
    auth_tx: mpsc::Sender<AgentAuthRequest>,
}

impl EnrollmentAgent {
    /// Creates a new enrollment agent bridging auth requests to the supplied channel.
    pub fn new(auth_tx: mpsc::Sender<AgentAuthRequest>) -> Self {
        Self { auth_tx }
    }
}

#[interface(name = "org.bluez.Agent1")]
impl EnrollmentAgent {
    fn release(&self) {
        tracing::debug!("BlueZ Agent1 released");
    }

    async fn request_pin_code(&self, device: OwnedObjectPath) -> fdo::Result<String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.auth_tx
            .send(AgentAuthRequest::PinCode {
                device: device.as_str().to_string(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| fdo::Error::Failed("agent bridge disconnected".into()))?;

        match reply_rx.await {
            Ok(Some(pin)) => Ok(pin),
            _ => Err(fdo::Error::Failed("PIN request cancelled".into())),
        }
    }

    fn display_pin_code(&self, device: OwnedObjectPath, pincode: &str) {
        let masked_device = mask_device_path(device.as_str());
        tracing::info!(device = %masked_device, pincode = %pincode, "BlueZ requested PIN display");
    }

    async fn request_passkey(&self, device: OwnedObjectPath) -> fdo::Result<u32> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.auth_tx
            .send(AgentAuthRequest::Passkey {
                device: device.as_str().to_string(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| fdo::Error::Failed("agent bridge disconnected".into()))?;

        match reply_rx.await {
            Ok(Some(passkey)) => Ok(passkey),
            _ => Err(fdo::Error::Failed("passkey request cancelled".into())),
        }
    }

    fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, entered: u16) {
        let masked_device = mask_device_path(device.as_str());
        tracing::info!(
            device = %masked_device,
            passkey = passkey,
            entered = entered,
            "BlueZ requested passkey display"
        );
    }

    async fn request_confirmation(&self, device: OwnedObjectPath, passkey: u32) -> fdo::Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.auth_tx
            .send(AgentAuthRequest::Confirmation {
                device: device.as_str().to_string(),
                passkey,
                reply: reply_tx,
            })
            .await
            .map_err(|_| fdo::Error::Failed("agent bridge disconnected".into()))?;

        match reply_rx.await {
            Ok(true) => Ok(()),
            _ => Err(fdo::Error::Failed("pairing rejected by user".into())),
        }
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> fdo::Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.auth_tx
            .send(AgentAuthRequest::Authorization {
                device: device.as_str().to_string(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| fdo::Error::Failed("agent bridge disconnected".into()))?;

        match reply_rx.await {
            Ok(true) => Ok(()),
            _ => Err(fdo::Error::Failed("authorization rejected by user".into())),
        }
    }

    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: &str) -> fdo::Result<()> {
        Ok(())
    }

    fn cancel(&self) {
        tracing::debug!("BlueZ Agent1 cancelled pairing");
    }
}

fn mask_device_path(path: &str) -> String {
    if let Some(pos) = path.rfind("/dev_") {
        let dev_part = &path[pos + 5..];
        let mac_str = dev_part.replace('_', ":");
        let masked = crate::logging::mask_mac(&mac_str);
        format!("{}/dev_{}", &path[..pos], masked.replace(':', "_"))
    } else {
        crate::logging::mask_mac(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_device_path_redacts_mac_in_bluez_object_path() {
        assert_eq!(
            mask_device_path("/org/bluez/hci0/dev_12_34_56_78_9A_BC"),
            "/org/bluez/hci0/dev_.._9A_BC"
        );
        assert_eq!(mask_device_path("12:34:56:78:9A:BC"), "..:9A:BC");
    }
}
