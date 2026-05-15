//! ZCL Smart Energy Metering cluster (0x0702)
//!
//! Parses cumulative energy summation and scaling attributes.
//!
//! Source: Zigbee Cluster Library spec, cluster 0x0702
//! HA equivalent: homeassistant/components/zha/sensor.py SmartEnergySummation

use serde_json::{json, Value};

use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct MeteringCluster;

// Cluster 0x0702 – Smart Energy Metering
//   0x0000 – CurrentSummationDelivered (Uint48, scaled kWh)
//   0x0300 – UnitOfMeasure            (Enum8;  0 = kWh)
//   0x0301 – Multiplier               (Uint24)
//   0x0302 – Divisor                  (Uint24)
//
// Source: Zigbee Cluster Library spec §10.4

// Attribute IDs
// Source: Zigbee Cluster Library spec §10.4.2 (Attribute Identifiers)
const CURRENT_SUMMATION_DELIVERED: u16 = 0x0000; // Uint48 – cumulative kWh (scaled)
const MULTIPLIER: u16 = 0x0301;                   // Uint24 – numerator for unit scaling
const DIVISOR: u16 = 0x0302;                      // Uint24 – denominator for unit scaling

impl ClusterHandler for MeteringCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut summation: Option<f64> = None;
        // Default multiplier = 1, divisor = 1 (unscaled kWh)
        let mut multiplier: f64 = 1.0;
        let mut divisor: f64 = 1.0;

        for r in reports {
            match r.attr_id {
                CURRENT_SUMMATION_DELIVERED => {
                    // Uint48: raw cumulative count, scaled by multiplier/divisor
                    // Source: homeassistant/components/zha/sensor.py SmartEnergySummation
                    summation = r.value.as_f64();
                }
                MULTIPLIER => {
                    // Uint24 – ZCL spec default is 1
                    if let Some(v) = r.value.as_f64() {
                        if v > 0.0 {
                            multiplier = v;
                        }
                    }
                }
                DIVISOR => {
                    // Uint24 – ZCL spec default is 1; guard against divide-by-zero
                    if let Some(v) = r.value.as_f64() {
                        if v > 0.0 {
                            divisor = v;
                        }
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        if let Some(raw) = summation {
            // Apply scaling: energy_kWh = raw × multiplier / divisor
            // Source: homeassistant/components/zha/sensor.py SmartEnergySummation._raw_value
            let kwh = raw * multiplier / divisor;
            // Round to 3 decimal places (Wh precision)
            out.push(("energy".into(), json!((kwh * 1000.0).round() / 1000.0)));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zigbee::zcl::attribute::{AttributeReport, AttributeValue};

    #[test]
    fn summation_unscaled() {
        // Default multiplier=1, divisor=1 → raw value in kWh directly
        let reports = vec![AttributeReport {
            attr_id: CURRENT_SUMMATION_DELIVERED,
            value: AttributeValue::U48(12500), // 12.500 kWh
        }];
        let result = MeteringCluster.process_reports(&reports);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "energy");
        // No scaling → 12500 / 1 = 12500, but without divisor this is just the raw count
        assert_eq!(result[0].1, json!(12500.0));
    }

    #[test]
    fn summation_with_divisor() {
        // multiplier=1, divisor=1000 → value / 1000 = kWh
        let reports = vec![
            AttributeReport {
                attr_id: CURRENT_SUMMATION_DELIVERED,
                value: AttributeValue::U48(12500), // raw count
            },
            AttributeReport {
                attr_id: MULTIPLIER,
                value: AttributeValue::U24(1),
            },
            AttributeReport {
                attr_id: DIVISOR,
                value: AttributeValue::U24(1000),
            },
        ];
        let result = MeteringCluster.process_reports(&reports);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "energy");
        assert_eq!(result[0].1, json!(12.5)); // 12500 * 1 / 1000 = 12.5 kWh
    }

    #[test]
    fn no_summation_produces_empty() {
        // Divisor-only report without summation → no output
        let reports = vec![AttributeReport {
            attr_id: DIVISOR,
            value: AttributeValue::U24(1000),
        }];
        let result = MeteringCluster.process_reports(&reports);
        assert!(result.is_empty());
    }

    #[test]
    fn zero_divisor_ignored() {
        // A zero divisor must be ignored (guard against divide-by-zero)
        let reports = vec![
            AttributeReport {
                attr_id: CURRENT_SUMMATION_DELIVERED,
                value: AttributeValue::U48(10000),
            },
            AttributeReport {
                attr_id: DIVISOR,
                value: AttributeValue::U24(0), // invalid, must be ignored
            },
        ];
        // Should fall back to divisor=1.0, producing 10000.0 kWh
        let result = MeteringCluster.process_reports(&reports);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1, json!(10000.0));
    }
}
