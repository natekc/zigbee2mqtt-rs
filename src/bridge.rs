/// Bridge — ties coordinator, device registry, MQTT, and user event channel together.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::{Config, DeviceConfig};
use crate::coordinator::{open_coordinator, CoordinatorEvent, CoordinatorHandle};
use crate::database;
use crate::devices::{Device, DeviceRegistry};
use crate::error::Result;
use crate::events::{BridgeCommand, DeviceInfo, ZigbeeEvent};
use crate::homeassistant;
use crate::mqtt::{MqttBridge, MqttCommand};
use crate::zigbee::zcl;
use crate::zigbee::zcl::clusters::{color, level, on_off};
use crate::zigbee::{EndpointDesc, IeeeAddr};

// ── Internal command type ─────────────────────────────────────────────────────
//
// Normalises MqttCommand and BridgeCommand into one stream.  The event loop
// has a single command arm; no Option-based select! pending() gymnastics.

enum InternalCmd {
    MqttReconnected,
    PermitJoin(u8),
    SetDevice { name: String, payload: Value },
    GetDevice { name: String },
    Stop,
}

// ── Dispatcher ────────────────────────────────────────────────────────────────
//
// Owns all output handles.  Every output operation goes through a method here.
// The event loop and command handler never inspect whether MQTT or the notify
// channel is present; they call Dispatcher methods unconditionally.

struct Dispatcher {
    mqtt:            Option<MqttBridge>,
    notify_tx:       Option<mpsc::Sender<ZigbeeEvent>>,
    base_topic:      String,
    coord_ieee:      String,
    ha_enabled:      bool,
    log_level:       String,
    permit_join_cfg: bool,
}

impl Dispatcher {
    async fn device_joined(&self, ieee: IeeeAddr, nwk: u16) {
        if let Some(ref m) = self.mqtt {
            let _ = m.publish_bridge_log("info", &format!("Device joined: {ieee}")).await;
        }
        self.emit(ZigbeeEvent::DeviceJoined { ieee_addr: ieee, nwk_addr: nwk }).await;
    }

    async fn device_left(&self, ieee: IeeeAddr, devices: &DeviceRegistry) {
        if let Some(ref m) = self.mqtt {
            let _ = m.publish_bridge_log("info", &format!("Device left: {ieee}")).await;
            publish_device_list(devices, m).await;
        }
        self.emit(ZigbeeEvent::DeviceLeft { ieee_addr: ieee }).await;
    }

    // Called on every SimpleDescRsp (device list changed but interview may still
    // be in progress).
    async fn device_list_changed(&self, devices: &DeviceRegistry) {
        if let Some(ref m) = self.mqtt {
            publish_device_list(devices, m).await;
        }
    }

    // Called once when interview is complete.  Implies a device list update.
    async fn interview_complete(&self, dev: &Device, devices: &DeviceRegistry) {
        if let Some(ref m) = self.mqtt {
            publish_device_list(devices, m).await;
            if self.ha_enabled {
                homeassistant::publish_discovery(m, dev, &self.base_topic, &self.coord_ieee).await;
            }
        }
        self.emit(ZigbeeEvent::DeviceInterviewComplete { info: DeviceInfo::from(dev) }).await;
    }

    async fn state_changed(
        &self,
        name: &str,
        ieee: IeeeAddr,
        delta: serde_json::Map<String, Value>,
        full_state: &serde_json::Map<String, Value>,
    ) {
        if let Some(ref m) = self.mqtt {
            let _ = m.publish_device_state(name, &Value::Object(full_state.clone())).await;
        }
        self.emit(ZigbeeEvent::StateChanged { ieee_addr: ieee, delta }).await;
    }

    async fn permit_join_ack(&self, duration: u8) {
        if let Some(ref m) = self.mqtt {
            let _ = m.publish_bridge_log("info", &format!("Permit join: {duration}s")).await;
        }
    }

    async fn handle_set(
        &self,
        devices: &DeviceRegistry,
        coord: &CoordinatorHandle,
        trans_id: &mut u8,
        name: &str,
        payload: &Value,
    ) {
        if let Some(ref m) = self.mqtt {
            do_handle_set(devices, coord, m, trans_id, name, payload).await;
        }
    }

    async fn handle_get(&self, devices: &DeviceRegistry, name: &str) {
        let Some(ref m) = self.mqtt else { return };
        match devices.find_by_name(name) {
            Some(dev) => {
                let _ = m.publish_device_state(name, &Value::Object(dev.state)).await;
            }
            None => warn!("Get command for unknown device: {name}"),
        }
    }

    // Republish all retained MQTT state (called on connect/reconnect).
    async fn republish(&self, devices: &DeviceRegistry, coord: &CoordinatorHandle) {
        let Some(ref m) = self.mqtt else { return };

        let _ = m.publish_bridge_state(true).await;
        let _ = m.publish_bridge_info(&json!({
            "version":     env!("CARGO_PKG_VERSION"),
            "coordinator": {
                "ieee_address": self.coord_ieee,
                "type":         "z-Stack",
                "meta": {
                    "revision": coord.info.transport_rev,
                    "version":  coord.info.version,
                },
            },
            "log_level":  self.log_level,
            "permit_join": self.permit_join_cfg,
            "config": {},
        })).await;

        for dev in devices.all_devices() {
            let _ = m.publish_device_state(
                &dev.friendly_name,
                &Value::Object(dev.state.clone()),
            ).await;
            if self.ha_enabled && dev.interview_complete {
                homeassistant::publish_discovery(m, &dev, &self.base_topic, &self.coord_ieee).await;
            }
        }
        publish_device_list(devices, m).await;
    }

    async fn shutdown(&self) {
        if let Some(ref m) = self.mqtt {
            let _ = m.publish_bridge_state(false).await;
        }
    }

    // Low-level helper — send to channel, ignore if not connected.
    async fn emit(&self, event: ZigbeeEvent) {
        if let Some(ref tx) = self.notify_tx {
            let _ = tx.send(event).await;
        }
    }
}

// ── From<&Device> for DeviceInfo ──────────────────────────────────────────────
//
// Bridge module is the only site that knows both the internal Device and the
// public DeviceInfo; keeping this impl here avoids importing Device into
// events.rs and preserves the API boundary.

impl From<&Device> for DeviceInfo {
    fn from(dev: &Device) -> Self {
        Self {
            ieee_addr:     dev.ieee_addr,
            nwk_addr:      dev.nwk_addr,
            friendly_name: dev.friendly_name.clone(),
            manufacturer:  dev.manufacturer.clone(),
            model:         dev.model.clone(),
            endpoints:     dev.endpoints.clone(),
        }
    }
}

// ── Bridge ────────────────────────────────────────────────────────────────────

pub struct Bridge {
    cfg:         Config,
    config_path: PathBuf,
    devices:     Arc<DeviceRegistry>,
    notify_tx:   Option<mpsc::Sender<ZigbeeEvent>>,
    cmd_rx:      Option<mpsc::Receiver<BridgeCommand>>,
}

impl Bridge {
    pub fn new(cfg: Config, config_path: PathBuf) -> Self {
        Self {
            cfg,
            config_path,
            devices:   Arc::new(DeviceRegistry::new()),
            notify_tx: None,
            cmd_rx:    None,
        }
    }

    /// Attach a channel that receives `ZigbeeEvent`s as they occur.
    pub fn with_notify(mut self, tx: mpsc::Sender<ZigbeeEvent>) -> Self {
        self.notify_tx = Some(tx);
        self
    }

    /// Attach a channel from which the bridge reads `BridgeCommand`s.
    pub fn with_commands(mut self, rx: mpsc::Receiver<BridgeCommand>) -> Self {
        self.cmd_rx = Some(rx);
        self
    }

    /// Convenience constructor: wire both channels in one call.
    pub fn new_with_channels(
        cfg: Config,
        config_path: PathBuf,
    ) -> (Self, mpsc::Receiver<ZigbeeEvent>, mpsc::Sender<BridgeCommand>) {
        let (notify_tx, notify_rx) = mpsc::channel(64);
        let (cmd_tx, cmd_rx)       = mpsc::channel(16);
        let bridge = Bridge::new(cfg, config_path)
            .with_notify(notify_tx)
            .with_commands(cmd_rx);
        (bridge, notify_rx, cmd_tx)
    }

    pub async fn run(self) -> Result<()> {
        // ── Setup ─────────────────────────────────────────────────────────────
        self.import_database();
        self.apply_device_configs();
        log_registry(&self.devices);

        // Connect MQTT (optional; set mqtt.enabled=false in config to run broker-free).
        let (mqtt_opt, mqtt_rx_opt) = if self.cfg.mqtt.enabled {
            let (mqtt, rx) = MqttBridge::connect(&self.cfg.mqtt).await?;
            info!("MQTT connected (base_topic={})", self.cfg.mqtt.base_topic);
            (Some(mqtt), Some(rx))
        } else {
            info!("No MQTT config — running broker-free");
            (None, None)
        };

        let mut coord = open_coordinator(&self.cfg).await?;
        info!("Coordinator ready");

        let dispatcher = Dispatcher {
            base_topic:      self.cfg.mqtt.base_topic.clone(),
            coord_ieee:      coord.info.ieee_addr.map(|a| a.as_hex()).unwrap_or_default(),
            ha_enabled:      self.cfg.homeassistant,
            log_level:       self.cfg.advanced.log_level.clone(),
            permit_join_cfg: self.cfg.permit_join,
            mqtt:            mqtt_opt,
            notify_tx:       self.notify_tx,
        };

        dispatcher.republish(&self.devices, &coord).await;

        if self.cfg.permit_join {
            coord.permit_join(254).await?;
            info!("Permit join enabled (254 s)");
        }
        info!("Startup complete");

        // ── Command channel: merge MQTT + BridgeCommand into one stream ───────
        //
        // internal_tx is kept alive for the whole loop so the channel never
        // closes spontaneously.  Each translator task clones it and sends
        // InternalCmd::Stop if its source channel dies.
        let (internal_tx, mut internal_rx) = mpsc::channel::<InternalCmd>(64);
        if let Some(mqtt_rx) = mqtt_rx_opt {
            spawn_mqtt_translator(mqtt_rx, internal_tx.clone());
        }
        if let Some(bridge_rx) = self.cmd_rx {
            spawn_bridge_translator(bridge_rx, internal_tx.clone());
        }

        // ── Event loop ────────────────────────────────────────────────────────
        let devices      = Arc::clone(&self.devices);
        let device_cfgs  = self.cfg.devices.clone();
        let mut trans_id: u8 = 0;

        loop {
            tokio::select! {
                event = coord.events.recv() => match event {
                    None    => { error!("Coordinator event channel closed"); break; }
                    Some(e) => handle_coordinator_event(
                        e, &coord, &devices, &device_cfgs, &dispatcher, &mut trans_id,
                    ).await,
                },
                cmd = internal_rx.recv() => match cmd {
                    None | Some(InternalCmd::Stop) => break,
                    Some(cmd) => handle_cmd(cmd, &coord, &devices, &dispatcher, &mut trans_id).await,
                },
            }
        }

        dispatcher.shutdown().await;
        drop(internal_tx);
        Ok(())
    }

    fn import_database(&self) {
        if let Some(db_path) = database::find_database(&self.config_path) {
            let (devices, _) = database::load_database(&db_path, &self.cfg.devices);
            for dev in devices {
                self.devices.add(dev);
            }
        }
    }

    fn apply_device_configs(&self) {
        for (ieee_str, cfg) in &self.cfg.devices {
            if let Some(ieee) = IeeeAddr::from_hex(ieee_str) {
                let name     = cfg.friendly_name.clone().unwrap_or_else(|| ieee.as_hex());
                let disabled = cfg.disabled.unwrap_or(false);
                if let Some(mut dev) = self.devices.get_mut_by_ieee(&ieee) {
                    dev.friendly_name = name;
                    dev.disabled      = disabled;
                } else {
                    let mut dev = Device::new(ieee, 0);
                    dev.friendly_name = name;
                    dev.disabled      = disabled;
                    self.devices.add(dev);
                }
            }
        }
    }
}

// ── Translator tasks ──────────────────────────────────────────────────────────
//
// Each task converts one external command type into InternalCmd and forwards
// it to the shared internal channel.  Sending Stop when the source closes
// ensures the bridge loop exits cleanly if its command source disappears.

fn spawn_mqtt_translator(mut rx: mpsc::Receiver<MqttCommand>, tx: mpsc::Sender<InternalCmd>) {
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            let internal = match cmd {
                MqttCommand::Connected              => InternalCmd::MqttReconnected,
                MqttCommand::PermitJoin { duration } => InternalCmd::PermitJoin(duration),
                MqttCommand::SetDevice { friendly_name, payload } => {
                    InternalCmd::SetDevice { name: friendly_name, payload }
                }
                MqttCommand::GetDevice { friendly_name } => {
                    InternalCmd::GetDevice { name: friendly_name }
                }
            };
            if tx.send(internal).await.is_err() { break; }
        }
        // MQTT event loop exited — stop the bridge.
        let _ = tx.send(InternalCmd::Stop).await;
    });
}

fn spawn_bridge_translator(mut rx: mpsc::Receiver<BridgeCommand>, tx: mpsc::Sender<InternalCmd>) {
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            let internal = match cmd {
                BridgeCommand::PermitJoin { duration } => InternalCmd::PermitJoin(duration),
                BridgeCommand::Stop                    => InternalCmd::Stop,
            };
            if tx.send(internal).await.is_err() { break; }
        }
        // Consumer dropped the command sender — stop the bridge.
        let _ = tx.send(InternalCmd::Stop).await;
    });
}

// ── Coordinator event handler ─────────────────────────────────────────────────

async fn handle_coordinator_event(
    event:       CoordinatorEvent,
    coord:       &CoordinatorHandle,
    devices:     &Arc<DeviceRegistry>,
    device_cfgs: &HashMap<String, DeviceConfig>,
    dispatcher:  &Dispatcher,
    trans_id:    &mut u8,
) {
    match event {
        CoordinatorEvent::DeviceJoined { ieee_addr, nwk_addr } => {
            info!("Device joined: {ieee_addr} (0x{nwk_addr:04X})");
            if devices.get_by_ieee(&ieee_addr).is_none() {
                devices.add(make_device(ieee_addr, nwk_addr, device_cfgs));
            } else {
                devices.update_nwk_addr(&ieee_addr, nwk_addr);
            }
            dispatcher.device_joined(ieee_addr, nwk_addr).await;
            if let Err(e) = coord.request_active_eps(nwk_addr).await {
                warn!("request_active_eps: {e}");
            }
        }

        CoordinatorEvent::DeviceLeft { ieee_addr } => {
            info!("Device left: {ieee_addr}");
            devices.remove_by_ieee(&ieee_addr);
            dispatcher.device_left(ieee_addr, devices).await;
        }

        CoordinatorEvent::AddressResolved { ieee_addr, nwk_addr } => {
            debug!("Address resolved: {ieee_addr} -> 0x{nwk_addr:04X}");
            if devices.get_by_ieee(&ieee_addr).is_some() {
                devices.update_nwk_addr(&ieee_addr, nwk_addr);
            } else {
                devices.add(make_device(ieee_addr, nwk_addr, device_cfgs));
            }
            if devices.get_by_ieee(&ieee_addr).is_some_and(|d| !d.interview_complete) {
                if let Err(e) = coord.request_active_eps(nwk_addr).await {
                    warn!("request_active_eps: {e}");
                }
            }
        }

        CoordinatorEvent::ActiveEpRsp { nwk_addr, endpoints } => {
            debug!("Active EPs 0x{nwk_addr:04X}: {endpoints:?}");
            for ep in endpoints {
                if let Err(e) = coord.request_simple_desc(nwk_addr, ep).await {
                    warn!("request_simple_desc: {e}");
                }
            }
        }

        CoordinatorEvent::SimpleDescRsp {
            nwk_addr, endpoint, profile_id, device_id,
            input_clusters, output_clusters,
        } => {
            debug!("SimpleDesc 0x{nwk_addr:04X} ep={endpoint} clusters={input_clusters:?}");

            let ep_desc = EndpointDesc {
                endpoint,
                profile_id,
                device_id,
                input_clusters: input_clusters.clone(),
                output_clusters,
            };

            let mut interview_just_completed = false;
            if let Some(mut dev) = devices.get_mut_by_nwk(nwk_addr) {
                dev.endpoints.retain(|e| e.endpoint != endpoint);
                dev.endpoints.push(ep_desc);
                if !dev.interview_complete && !dev.endpoints.is_empty() {
                    dev.interview_complete    = true;
                    interview_just_completed  = true;
                    info!("Interview complete for {}", dev.display_name());
                }
            }

            // Read basic cluster attributes (manufacturer, model, power source).
            if input_clusters.contains(&0x0000) {
                let payload = zcl::frame::read_attributes_payload(&[0x0004, 0x0005, 0x0007, 0x4000]);
                *trans_id = trans_id.wrapping_add(1);
                if let Err(e) = coord.send_zcl(nwk_addr, endpoint, 0x0000, *trans_id, payload).await {
                    warn!("ZCL read attributes: {e}");
                }
            }

            if interview_just_completed {
                if let Some(dev) = devices.get_by_nwk(nwk_addr) {
                    // interview_complete implies device_list_changed, so no separate call.
                    dispatcher.interview_complete(&dev, devices).await;
                }
            } else {
                dispatcher.device_list_changed(devices).await;
            }
        }

        CoordinatorEvent::Message { src_addr, src_ep, cluster_id, link_quality, data } => {
            debug!("AF msg 0x{src_addr:04X} ep={src_ep} cluster=0x{cluster_id:04X} lqi={link_quality}");

            if devices.get_by_nwk(src_addr).is_none() {
                if let Err(e) = coord.request_ieee_addr(src_addr).await {
                    warn!("request_ieee_addr: {e}");
                }
            }

            if cluster_id == 0x0000 {
                handle_basic_cluster_response(devices, src_addr, &data);
            }

            if let Ok(Some(zcl_msg)) = zcl::parse_message(cluster_id, &data) {
                if let Some(mut dev) = devices.get_mut_by_nwk(src_addr) {
                    let delta = zcl_msg.values.clone();
                    dev.merge_state(zcl_msg.values);
                    dev.state.insert("linkquality".into(), json!(link_quality));
                    dev.state.insert("last_seen".into(),   json!(now_iso8601()));
                    let full_state = dev.state.clone();
                    let ieee       = dev.ieee_addr;
                    let name       = dev.friendly_name.clone();
                    drop(dev); // release DashMap shard lock before awaiting
                    dispatcher.state_changed(&name, ieee, delta, &full_state).await;
                }
            }
        }
    }
}

// ── Command handler ───────────────────────────────────────────────────────────

async fn handle_cmd(
    cmd:        InternalCmd,
    coord:      &CoordinatorHandle,
    devices:    &Arc<DeviceRegistry>,
    dispatcher: &Dispatcher,
    trans_id:   &mut u8,
) {
    match cmd {
        InternalCmd::MqttReconnected => {
            info!("MQTT session restored, republishing retained state");
            dispatcher.republish(devices, coord).await;
        }
        InternalCmd::PermitJoin(duration) => {
            info!("Permit join: {duration}s");
            if let Err(e) = coord.permit_join(duration).await {
                warn!("permit_join: {e}");
            }
            dispatcher.permit_join_ack(duration).await;
        }
        InternalCmd::SetDevice { name, payload } => {
            info!("Set: {name} -> {payload}");
            dispatcher.handle_set(devices, coord, trans_id, &name, &payload).await;
        }
        InternalCmd::GetDevice { name } => {
            debug!("Get: {name}");
            dispatcher.handle_get(devices, &name).await;
        }
        InternalCmd::Stop => unreachable!("Stop is handled in the loop"),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_device(ieee: IeeeAddr, nwk: u16, cfgs: &HashMap<String, DeviceConfig>) -> Device {
    let mut dev = Device::new(ieee, nwk);
    if let Some(cfg) = cfgs.get(&ieee.as_hex()) {
        if let Some(ref name) = cfg.friendly_name {
            dev.friendly_name = name.clone();
        }
        dev.disabled = cfg.disabled.unwrap_or(false);
    }
    dev
}

fn log_registry(devices: &DeviceRegistry) {
    let all = devices.all_devices();
    info!(
        "Device registry: {} devices ({} interviewed)",
        all.len(),
        all.iter().filter(|d| d.interview_complete).count(),
    );
    for dev in &all {
        info!(
            "  {} ({}) nwk=0x{:04X} eps={} interviewed={} model={:?}",
            dev.friendly_name, dev.ieee_addr, dev.nwk_addr,
            dev.endpoints.len(), dev.interview_complete,
            dev.model.as_deref().unwrap_or("-"),
        );
    }
}

async fn publish_device_list(devices: &DeviceRegistry, mqtt: &MqttBridge) {
    let list: Vec<_> = devices.all_devices().iter().map(|d| d.to_z2m_device_json()).collect();
    if let Err(e) = mqtt.publish_bridge_devices(&json!(list)).await {
        warn!("publish_bridge_devices: {e}");
    }
}

fn handle_basic_cluster_response(devices: &DeviceRegistry, src_addr: u16, data: &[u8]) {
    let Ok(Some(zcl_msg)) = zcl::parse_message(0x0000, data) else { return };
    if let Some(mut dev) = devices.get_mut_by_nwk(src_addr) {
        for (key, value) in &zcl_msg.values {
            match key.as_str() {
                "manufacturer" => dev.manufacturer  = value.as_str().map(str::to_owned),
                "model"        => dev.model         = value.as_str().map(str::to_owned),
                "power_source" => dev.power_source  = value.as_str().map(str::to_owned),
                "sw_build_id"  => dev.sw_build_id   = value.as_str().map(str::to_owned),
                _              => {}
            }
        }
    }
}

// ── Device set command ────────────────────────────────────────────────────────

async fn do_handle_set(
    devices:  &DeviceRegistry,
    coord:    &CoordinatorHandle,
    mqtt:     &MqttBridge,
    trans_id: &mut u8,
    name:     &str,
    payload:  &Value,
) {
    let dev = match devices.find_by_name(name) {
        Some(d) => d,
        None    => { warn!("Set command for unknown device: {name}"); return; }
    };
    if dev.nwk_addr == 0 || !dev.interview_complete {
        warn!(
            "Device {name} not yet available (nwk=0x{:04X} interviewed={})",
            dev.nwk_addr, dev.interview_complete
        );
        return;
    }
    let nwk_addr  = dev.nwk_addr;
    let endpoints = dev.endpoints.clone();
    drop(dev);

    let mut optimistic = serde_json::Map::new();

    if let Some(state_val) = payload.get("state") {
        let s = state_val.as_str().unwrap_or("");
        if let Some(ep) = find_ep_with_cluster(&endpoints, 0x0006) {
            if let Some(p) = on_off::set_state_payload(*trans_id, s) {
                *trans_id = trans_id.wrapping_add(1);
                if let Err(e) = coord.send_zcl(nwk_addr, ep, 0x0006, *trans_id, p).await {
                    warn!("on/off command: {e}");
                }
                optimistic.insert("state".into(), json!(s.to_uppercase()));
            }
        }
    }

    if let Some(brightness) = payload.get("brightness").and_then(|v| v.as_u64()) {
        if let Some(ep) = find_ep_with_cluster(&endpoints, 0x0008) {
            let lvl = brightness.min(254) as u8;
            let t   = transition_time(payload);
            let p   = level::move_to_level_payload(*trans_id, lvl, t);
            *trans_id = trans_id.wrapping_add(1);
            if let Err(e) = coord.send_zcl(nwk_addr, ep, 0x0008, *trans_id, p).await {
                warn!("level command: {e}");
            }
            optimistic.insert("brightness".into(), json!(lvl));
            if !optimistic.contains_key("state") {
                optimistic.insert("state".into(), json!("ON"));
            }
        }
    }

    if let Some(ct) = payload.get("color_temp").and_then(|v| v.as_u64()) {
        if let Some(ep) = find_ep_with_cluster(&endpoints, 0x0300) {
            let t = transition_time(payload);
            let p = color::move_to_color_temp_payload(*trans_id, ct as u16, t);
            *trans_id = trans_id.wrapping_add(1);
            if let Err(e) = coord.send_zcl(nwk_addr, ep, 0x0300, *trans_id, p).await {
                warn!("color temp command: {e}");
            }
            optimistic.insert("color_temp".into(), json!(ct));
            optimistic.insert("color_mode".into(), json!("color_temp"));
        }
    }

    if let Some(color_obj) = payload.get("color").and_then(|v| v.as_object()) {
        if let Some(ep) = find_ep_with_cluster(&endpoints, 0x0300) {
            let t = transition_time(payload);
            if let (Some(x), Some(y)) = (
                color_obj.get("x").and_then(|v| v.as_f64()),
                color_obj.get("y").and_then(|v| v.as_f64()),
            ) {
                let p = color::move_to_color_xy_payload(*trans_id, x, y, t);
                *trans_id = trans_id.wrapping_add(1);
                if let Err(e) = coord.send_zcl(nwk_addr, ep, 0x0300, *trans_id, p).await {
                    warn!("color XY command: {e}");
                }
                optimistic.insert("color".into(), json!({"x": x, "y": y}));
                optimistic.insert("color_mode".into(), json!("xy"));
            } else if let (Some(h), Some(s)) = (
                color_obj.get("hue").and_then(|v| v.as_f64()),
                color_obj.get("saturation").and_then(|v| v.as_f64()),
            ) {
                let zcl_hue = ((h / 360.0) * 254.0).round() as u8;
                let zcl_sat = ((s / 100.0) * 254.0).round() as u8;
                let p = color::move_to_hue_sat_payload(*trans_id, zcl_hue, zcl_sat, t);
                *trans_id = trans_id.wrapping_add(1);
                if let Err(e) = coord.send_zcl(nwk_addr, ep, 0x0300, *trans_id, p).await {
                    warn!("color HS command: {e}");
                }
                optimistic.insert("color".into(), json!({"hue": h, "saturation": s}));
                optimistic.insert("color_mode".into(), json!("hs"));
            }
        }
    }

    if !optimistic.is_empty() {
        if let Some(mut dev) = devices.get_mut_by_nwk(nwk_addr) {
            dev.merge_state(optimistic.clone());
            dev.state.insert("last_seen".into(), json!(now_iso8601()));
        }
        if let Some(dev) = devices.find_by_name(name) {
            let _ = mqtt.publish_device_state(name, &Value::Object(dev.state)).await;
        }
    }
}

// ── Utility functions ─────────────────────────────────────────────────────────

fn find_ep_with_cluster(endpoints: &[EndpointDesc], cluster_id: u16) -> Option<u8> {
    endpoints
        .iter()
        .find(|e| e.input_clusters.contains(&cluster_id))
        .map(|e| e.endpoint)
}

fn transition_time(payload: &Value) -> u16 {
    payload
        .get("transition")
        .and_then(|v| v.as_f64())
        .map(|s| (s.max(0.0) * 10.0).min(65535.0) as u16)
        .unwrap_or(0)
}

fn now_iso8601() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs        = dur.as_secs();
    let days        = secs / 86400;
    let time_of_day = secs % 86400;
    let hours       = time_of_day / 3600;
    let minutes     = (time_of_day % 3600) / 60;
    let seconds     = time_of_day % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}+00:00")
}

fn days_to_ymd(days_since_epoch: u64) -> (u64, u64, u64) {
    let z   = days_since_epoch + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y   = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp  = (5 * doy + 2) / 153;
    let d   = doy - (153 * mp + 2) / 5 + 1;
    let m   = if mp < 10 { mp + 3 } else { mp - 9 };
    let y   = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ieee_addr_valid() {
        let addr = IeeeAddr::from_hex("0x00158D0001020304").unwrap();
        assert_eq!(addr.as_hex(), "0x00158d0001020304");
    }

    #[test]
    fn parse_ieee_addr_lowercase() {
        let addr = IeeeAddr::from_hex("0xec1bbdfffeaa66db").unwrap();
        assert_eq!(addr.as_hex().to_lowercase(), "0xec1bbdfffeaa66db");
    }

    #[test]
    fn parse_ieee_addr_invalid() {
        assert!(IeeeAddr::from_hex("0x1234").is_none());
        assert!(IeeeAddr::from_hex("not_hex").is_none());
    }

    #[test]
    fn now_iso8601_format() {
        let ts = now_iso8601();
        assert!(ts.contains('T'));
        assert!(ts.ends_with("+00:00"));
        assert_eq!(ts.len(), 25);
    }

    #[test]
    fn days_to_ymd_epoch() {
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        let (y, m, d) = days_to_ymd(19737);
        assert_eq!((y, m, d), (2024, 1, 15));
    }

    #[test]
    fn transition_time_from_payload() {
        let p = json!({"state": "ON", "transition": 1.5});
        assert_eq!(transition_time(&p), 15);
    }

    #[test]
    fn transition_time_default() {
        let p = json!({"state": "ON"});
        assert_eq!(transition_time(&p), 0);
    }

    #[test]
    fn find_ep_with_existing_cluster() {
        let eps = vec![
            EndpointDesc {
                endpoint: 1, profile_id: 0x0104, device_id: 0,
                input_clusters: vec![0x0000, 0x0006], output_clusters: vec![],
            },
            EndpointDesc {
                endpoint: 2, profile_id: 0x0104, device_id: 0,
                input_clusters: vec![0x0402], output_clusters: vec![],
            },
        ];
        assert_eq!(find_ep_with_cluster(&eps, 0x0006), Some(1));
        assert_eq!(find_ep_with_cluster(&eps, 0x0402), Some(2));
        assert_eq!(find_ep_with_cluster(&eps, 0x9999), None);
    }
}
