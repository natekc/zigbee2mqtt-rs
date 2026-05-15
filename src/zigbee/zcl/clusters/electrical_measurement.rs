//! ZCL Electrical Measurement cluster (0x0B04)
//!
//! Parses instantaneous power, RMS voltage, and RMS current with AC divisor scaling.
//!
//! Source: Zigbee Cluster Library spec, cluster 0x0B04
//! HA equivalent: homeassistant/components/zha/sensor.py ElectricalMeasurement

use serde_json::{json, Value};

use super::super::attribute::AttributeReport;
use super::ClusterHandler;

pub struct ElectricalMeasurementCluster;

// Cluster 0x0B04 – Electrical Measurement
//   0x0505 – RMSVoltage          (Uint16, V, scaled by ACVoltageDivisor)
//   0x0508 – RMSCurrent          (Uint16, A, scaled by ACCurrentDivisor)
//   0x050B – ActivePower         (Int16,  W, scaled by ACPowerDivisor)
//   0x0601 – ACVoltageDivisor    (Uint16)
//   0x0603 – ACCurrentDivisor    (Uint16)
//   0x0605 – ACPowerDivisor      (Uint16)
//
// Source: Zigbee Cluster Library spec §4.3 (Electrical Measurement cluster)

// Attribute IDs
// Source: Zigbee Cluster Library spec §4.3.2.3 (AC Measurement attribute set)
const RMS_VOLTAGE: u16 = 0x0505;         // Uint16 – RMS voltage in scaled units
const RMS_CURRENT: u16 = 0x0508;         // Uint16 – RMS current in scaled units
const ACTIVE_POWER: u16 = 0x050B;        // Int16  – Active (real) power in scaled units
const AC_VOLTAGE_DIVISOR: u16 = 0x0601;  // Uint16 – Voltage scale denominator
const AC_CURRENT_DIVISOR: u16 = 0x0603;  // Uint16 – Current scale denominator
const AC_POWER_DIVISOR: u16 = 0x0605;    // Uint16 – Power scale denominator

impl ClusterHandler for ElectricalMeasurementCluster {
    fn process_reports(&self, reports: &[AttributeReport]) -> Vec<(String, Value)> {
        let mut voltage_raw: Option<f64> = None;
        let mut current_raw: Option<f64> = None;
        let mut power_raw: Option<f64> = None;
        // Default divisors = 1 (unscaled); ZCL spec §4.3.2.3
        let mut volt_div: f64 = 1.0;
        let mut curr_div: f64 = 1.0;
        let mut power_div: f64 = 1.0;

        for r in reports {
            match r.attr_id {
                RMS_VOLTAGE => {
                    // Uint16 – V before divisor
                    // Source: homeassistant/components/zha/sensor.py ElectricalMeasurementRMSVoltage
                    voltage_raw = r.value.as_f64();
                }
                RMS_CURRENT => {
                    // Uint16 – A before divisor
                    // Source: homeassistant/components/zha/sensor.py ElectricalMeasurementRMSCurrent
                    current_raw = r.value.as_f64();
                }
                ACTIVE_POWER => {
                    // Int16 – W before divisor (signed; negative = power exported)
                    // Source: homeassistant/components/zha/sensor.py ElectricalMeasurementActivePower
                    power_raw = r.value.as_f64();
                }
                AC_VOLTAGE_DIVISOR => {
                    if let Some(v) = r.value.as_f64() {
                        if v > 0.0 {
                            volt_div = v;
                        }
                    }
                }
                AC_CURRENT_DIVISOR => {
                    if let Some(v) = r.value.as_f64() {
                        if v > 0.0 {
                            curr_div = v;
                        }
                    }
                }
                AC_POWER_DIVISOR => {
                    if let Some(v) = r.value.as_f64() {
                        if v > 0.0 {
                            power_div = v;
                        }
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();

        if let Some(raw) = voltage_raw {
            // Unit: V (volts), 1 decimal place
            // Source: homeassistant/const.py UnitOfElectricPotential.VOLT = "V"
            let v = raw / volt_div;
            out.push(("voltage".into(), json!((v * 10.0).round() / 10.0)));
        }
        if let Some(raw) = current_raw {
            // Unit: A (amperes), 3 decimal places
            // Source: homeassistant/const.py UnitOfElectricCurrent.AMPERE = "A"
            let a = raw / curr_div;
            out.push(("current".into(), json!((a * 1000.0).round() / 1000.0)));
        }
        if let Some(raw) = power_raw {
            // Unit: W (watts), 1 decimal place
            // Source: homeassistant/const.py UnitOfPower.WATT = "W"
            let w = raw / power_div;
            out.push(("power".into(), json!((w * 10.0).round() / 10.0)));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zigbee::zcl::attribute::{AttributeReport, AttributeValue};

    #[test]
    fn active_power_unscaled() {
        // Default divisor=1 → raw value in W directly
        let reports = vec![AttributeReport {
            attr_id: ACTIVE_POWER,
            value: AttributeValue::I16(230), // 230 W
        }];
        let result = ElectricalMeasurementCluster.process_reports(&reports);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "power");
        assert_eq!(result[0].1, json!(230.0));
    }

    #[test]
    fn active_power_with_divisor() {
        // divisor=10 → 2300 raw / 10 = 230 W
        let reports = vec![
            AttributeReport {
                attr_id: ACTIVE_POWER,
                value: AttributeValue::I16(2300),
            },
            AttributeReport {
                attr_id: AC_POWER_DIVISOR,
                value: AttributeValue::U16(10),
            },
        ];
        let result = ElectricalMeasurementCluster.process_reports(&reports);
        assert_eq!(result.iter().find(|(k, _)| k == "power").map(|(_, v)| v), Some(&json!(230.0)));
    }

    #[test]
    fn rms_voltage_with_divisor() {
        // divisor=10 → 2300 raw / 10 = 230.0 V
        let reports = vec![
            AttributeReport {
                attr_id: RMS_VOLTAGE,
                value: AttributeValue::U16(2300),
            },
            AttributeReport {
                attr_id: AC_VOLTAGE_DIVISOR,
                value: AttributeValue::U16(10),
            },
        ];
        let result = ElectricalMeasurementCluster.process_reports(&reports);
        assert_eq!(result.iter().find(|(k, _)| k == "voltage").map(|(_, v)| v), Some(&json!(230.0)));
    }

    #[test]
    fn rms_current_with_divisor() {
        // divisor=1000 → 1500 raw / 1000 = 1.5 A
        let reports = vec![
            AttributeReport {
                attr_id: RMS_CURRENT,
                value: AttributeValue::U16(1500),
            },
            AttributeReport {
                attr_id: AC_CURRENT_DIVISOR,
                value: AttributeValue::U16(1000),
            },
        ];
        let result = ElectricalMeasurementCluster.process_reports(&reports);
        assert_eq!(result.iter().find(|(k, _)| k == "current").map(|(_, v)| v), Some(&json!(1.5)));
    }

    #[test]
    fn all_three_measurements() {
        let reports = vec![
            AttributeReport { attr_id: ACTIVE_POWER,    value: AttributeValue::I16(2300) },
            AttributeReport { attr_id: RMS_VOLTAGE,     value: AttributeValue::U16(2300) },
            AttributeReport { attr_id: RMS_CURRENT,     value: AttributeValue::U16(1500) },
            AttributeReport { attr_id: AC_POWER_DIVISOR,   value: AttributeValue::U16(10) },
            AttributeReport { attr_id: AC_VOLTAGE_DIVISOR, value: AttributeValue::U16(10) },
            AttributeReport { attr_id: AC_CURRENT_DIVISOR, value: AttributeValue::U16(1000) },
        ];
        let result = ElectricalMeasurementCluster.process_reports(&reports);
        assert!(result.iter().any(|(k, v)| k == "power"   && *v == json!(230.0)));
        assert!(result.iter().any(|(k, v)| k == "voltage" && *v == json!(230.0)));
        assert!(result.iter().any(|(k, v)| k == "current" && *v == json!(1.5)));
    }

    #[test]
    fn negative_power_exported() {
        // Negative active power = energy exported to grid
        let reports = vec![AttributeReport {
            attr_id: ACTIVE_POWER,
            value: AttributeValue::I16(-500), // -500 W exported
        }];
        let result = ElectricalMeasurementCluster.process_reports(&reports);
        assert_eq!(result.iter().find(|(k, _)| k == "power").map(|(_, v)| v), Some(&json!(-500.0)));
    }

    #[test]
    fn zero_divisor_ignored() {
        // A zero divisor must be ignored
        let reports = vec![
            AttributeReport { attr_id: ACTIVE_POWER,    value: AttributeValue::I16(100) },
            AttributeReport { attr_id: AC_POWER_DIVISOR, value: AttributeValue::U16(0) }, // invalid
        ];
        let result = ElectricalMeasurementCluster.process_reports(&reports);
        // Falls back to divisor=1 → 100.0 W
        assert_eq!(result.iter().find(|(k, _)| k == "power").map(|(_, v)| v), Some(&json!(100.0)));
    }
}
