pub mod bridge;
pub mod config;
pub mod coordinator;
pub mod database;
pub mod devices;
pub mod error;
pub mod events;
pub mod homeassistant;
pub mod mqtt;
pub mod zigbee;

// Convenience re-exports for library consumers.
pub use devices::Device;
pub use events::{BridgeCommand, DeviceInfo, ZigbeeEvent};
pub use zigbee::{EndpointDesc, IeeeAddr, NwkAddr};
