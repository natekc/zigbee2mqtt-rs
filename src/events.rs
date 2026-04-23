/// Events delivered to library consumers via the notify channel.
use serde_json::Map;

use crate::zigbee::{EndpointDesc, IeeeAddr, NwkAddr};

/// A snapshot of device information delivered when interview completes.
///
/// This is a stable, purpose-built view.  Callers should not depend on the
/// internal `Device` type directly; that type is subject to change.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub ieee_addr:     IeeeAddr,
    pub nwk_addr:      NwkAddr,
    pub friendly_name: String,
    pub manufacturer:  Option<String>,
    pub model:         Option<String>,
    /// Endpoint descriptors gathered during the ZDO interview.
    pub endpoints:     Vec<EndpointDesc>,
}

/// Events produced by the bridge and sent on the notify channel.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ZigbeeEvent {
    /// A device sent its first join announcement.  Interview has not yet run;
    /// the device record is minimal at this point.
    DeviceJoined { ieee_addr: IeeeAddr, nwk_addr: NwkAddr },

    /// A device sent a leave announcement and has been removed from the
    /// registry.
    DeviceLeft { ieee_addr: IeeeAddr },

    /// The ZDO interview is complete: endpoints and cluster IDs are known.
    /// This is the primary signal to create entities for the device.
    DeviceInterviewComplete { info: DeviceInfo },

    /// New attribute values reported in this message (delta only).
    /// Contains only the keys that changed in this report, not the full
    /// accumulated device state.  Callers that need the full state should
    /// maintain their own merge.
    StateChanged {
        ieee_addr: IeeeAddr,
        delta:     Map<String, serde_json::Value>,
    },
}

/// Commands sent from a library consumer back to the bridge.
#[derive(Debug)]
#[non_exhaustive]
pub enum BridgeCommand {
    /// Enable device pairing for `duration` seconds (0 = disable).
    PermitJoin { duration: u8 },
    /// Shut down the bridge gracefully.
    Stop,
}
