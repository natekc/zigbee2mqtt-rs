use serde_json::{json, Value};
use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct IasZoneCluster;

// Cluster 0x0500 – IAS Zone (door/window sensors, motion sensors, smoke detectors)
//   Attribute 0x0000 – ZoneState  (Enum8)
//   Attribute 0x0001 – ZoneType   (Enum16)
//   Attribute 0x0002 – ZoneStatus (Bitmap16)
//
// Cluster-specific commands (server → client):
//   0x00 – Zone Status Change Notification

// Source: Zigbee Cluster Library spec, Table 8-1 (IAS Zone cluster attribute IDs)
const ZONE_TYPE:   u16 = 0x0001; // Enum16 — identifies the kind of sensor
const ZONE_STATUS: u16 = 0x0002; // Bitmap16 — alarm/tamper/battery bits

const ALARM1:   u16 = 0x0001;
const TAMPER:   u16 = 0x0004;
const BATTERY:  u16 = 0x0008;
const TROUBLE:  u16 = 0x0040;

impl ClusterHandler for IasZoneCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for r in reports {
            match r.attr_id {
                ZONE_TYPE => {
                    // Source: Zigbee spec IAS Zone cluster attribute 0x0001 ZoneType (Enum16).
                    // Emit as a numeric u16 so consumers can map it to HA device_class.
                    if let Some(v) = r.value.as_f64() {
                        out.push(("zone_type".into(), json!(v as u16)));
                    }
                }
                ZONE_STATUS => {
                    if let Some(v) = r.value.as_f64() {
                        out.extend(decode_zone_status(v as u16));
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn process_command(&self, command_id: u8, payload: &[u8]) -> Vec<(String, Value)> {
        // 0x00 = Zone Status Change Notification
        // payload: zone_status (u16) | extended_status (u8) | zone_id (u8) | delay (u16)
        if command_id == 0x00 && payload.len() >= 2 {
            let zone_status = u16::from_le_bytes([payload[0], payload[1]]);
            return decode_zone_status(zone_status);
        }
        vec![]
    }
}

fn decode_zone_status(status: u16) -> Vec<(String, Value)> {
    vec![
        ("contact".into(), json!((status & ALARM1) == 0)), // contact closed = no alarm
        ("tamper".into(), json!((status & TAMPER) != 0)),
        ("battery_low".into(), json!((status & BATTERY) != 0)),
        ("trouble".into(), json!((status & TROUBLE) != 0)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zigbee::zcl::attribute::{AttributeReport, AttributeValue};

    #[test]
    fn zone_status_closed() {
        let reports = vec![AttributeReport {
            attr_id: ZONE_STATUS,
            value: AttributeValue::U16(0x0000), // all clear
        }];
        let result = IasZoneCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "contact" && v == &json!(true)));
        assert!(result.iter().any(|(k, v)| k == "tamper" && v == &json!(false)));
    }

    #[test]
    fn zone_status_open() {
        let reports = vec![AttributeReport {
            attr_id: ZONE_STATUS,
            value: AttributeValue::U16(0x0001), // ALARM1 = open
        }];
        let result = IasZoneCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "contact" && v == &json!(false)));
    }

    #[test]
    fn zone_status_tamper() {
        let reports = vec![AttributeReport {
            attr_id: ZONE_STATUS,
            value: AttributeValue::U16(0x0004), // TAMPER
        }];
        let result = IasZoneCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "tamper" && v == &json!(true)));
    }

    #[test]
    fn zone_status_change_notification() {
        // Command 0x00 with zone_status = open + tamper
        let payload = [0x05, 0x00, 0x00, 0x01, 0x00, 0x00]; // status=0x0005
        let result = IasZoneCluster.process_command(0x00, &payload);
        assert!(result.iter().any(|(k, v)| k == "contact" && v == &json!(false)));
        assert!(result.iter().any(|(k, v)| k == "tamper" && v == &json!(true)));
    }

    // Source: Zigbee spec IAS Zone cluster, Table 8-4 IAS Zone Type attribute values.
    #[test]
    fn zone_type_motion_sensor() {
        let reports = vec![AttributeReport {
            attr_id: ZONE_TYPE,
            value: AttributeValue::U16(0x000d), // Motion sensor
        }];
        let result = IasZoneCluster.process_reports(&reports);
        assert!(
            result.iter().any(|(k, v)| k == "zone_type" && v == &json!(0x000du16)),
            "zone_type should be emitted as a u16 value"
        );
    }

    #[test]
    fn zone_type_contact_switch() {
        let reports = vec![AttributeReport {
            attr_id: ZONE_TYPE,
            value: AttributeValue::U16(0x0015), // Contact switch (door/window)
        }];
        let result = IasZoneCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "zone_type" && v == &json!(0x0015u16)));
    }

    #[test]
    fn zone_type_and_status_in_same_report() {
        // Both ZoneType and ZoneStatus can appear in the same attribute report list.
        let reports = vec![
            AttributeReport { attr_id: ZONE_TYPE,   value: AttributeValue::U16(0x0015) },
            AttributeReport { attr_id: ZONE_STATUS,  value: AttributeValue::U16(0x0001) },
        ];
        let result = IasZoneCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "zone_type" && v == &json!(0x0015u16)));
        assert!(result.iter().any(|(k, v)| k == "contact" && v == &json!(false)));
    }
}
