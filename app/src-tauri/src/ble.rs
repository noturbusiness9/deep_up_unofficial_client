//! BLE-протокол кубика (см. ../../docs/PROTOCOL.md).
//! Пишем только в 98658F3A (управление). OAD-сервис не трогаем никогда.

use std::time::Duration;

use btleplug::api::{
    Central, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use serde::Serialize;
use uuid::Uuid;

pub const SVC_PROGRAM: Uuid = Uuid::from_u128(0x98658F38_E4E3_4AC4_8E14_B1B7FF024353);
pub const CHR_STATE: Uuid = Uuid::from_u128(0x98658F39_E4E3_4AC4_8E14_B1B7FF024353);
pub const CHR_CONTROL: Uuid = Uuid::from_u128(0x98658F3A_E4E3_4AC4_8E14_B1B7FF024353);
pub const SVC_SETTINGS: Uuid = Uuid::from_u128(0x9B5BFB12_20D7_4730_AA0C_993EA99FD4CD);
pub const SVC_OAD: Uuid = Uuid::from_u128(0xF000FFC0_0451_4000_B000_000000000000);
const CHR_BATTERY_LEVEL: Uuid = Uuid::from_u128(0x00002A19_0000_1000_8000_00805F9B34FB);
const CHR_BATTERY_STATE: Uuid = Uuid::from_u128(0x00002A1A_0000_1000_8000_00805F9B34FB);
const CHR_MODEL: Uuid = Uuid::from_u128(0x00002A24_0000_1000_8000_00805F9B34FB);
const CHR_FIRMWARE: Uuid = Uuid::from_u128(0x00002A26_0000_1000_8000_00805F9B34FB);

pub const CMD_PLAY: u8 = 0x1A;
pub const CMD_PAUSE: u8 = 0x2B;
pub const CMD_STOP: u8 = 0x3C;

/// Прошивка принимает длительность программы 1…24 ч.
pub const MIN_SECONDS: u32 = 3600;
pub const MAX_SECONDS: u32 = 86400;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProgramState {
    /// "awaiting" | "active" | "paused" | "unknown"
    pub state: String,
    pub program_id: u8,
    pub power: u8,
    pub percent: u8,
    /// Байты 4..7 — полная длительность, 8..11 — прошло (секунды); проверено на кубике.
    pub a: u32,
    pub b: u32,
    pub raw: String,
}

pub fn parse_state(data: &[u8]) -> ProgramState {
    let raw = data.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
    if data.len() != 12 {
        return ProgramState { state: "unknown".into(), raw, ..Default::default() };
    }
    let state = match data[0] {
        0x00 => "awaiting",
        0xAA => "active",
        0xBB => "paused",
        _ => "unknown",
    };
    ProgramState {
        state: state.into(),
        program_id: data[1],
        power: data[2],
        percent: data[3],
        a: u32::from_le_bytes(data[4..8].try_into().unwrap()),
        b: u32::from_le_bytes(data[8..12].try_into().unwrap()),
        raw,
    }
}

/// 8 байт для 98658F3A: [cmd][id][power][side][duration u32 LE].
pub fn control_packet(cmd: u8, program_id: u8, power: u8, side: u8, seconds: u32) -> [u8; 8] {
    let mut p = [0u8; 8];
    p[0] = cmd;
    if cmd == CMD_PLAY {
        p[1] = program_id;
        p[2] = power;
        p[3] = side;
        p[4..8].copy_from_slice(&seconds.to_le_bytes());
    }
    p
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Battery {
    pub level: Option<u8>,
    pub charging: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeviceInfo {
    pub name: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
}

pub async fn adapter() -> Result<Adapter, String> {
    let manager = Manager::new().await.map_err(|e| e.to_string())?;
    manager
        .adapters()
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .next()
        .ok_or_else(|| "Bluetooth-адаптер не найден".into())
}

fn is_cube_adv(services: &[Uuid]) -> bool {
    services.iter().any(|u| *u == SVC_PROGRAM || *u == SVC_SETTINGS || *u == SVC_OAD)
}

/// Ищет кубик по имени `deep.*`, рекламе сервисов или запомненному ID.
/// Перебор чужих устройств не делаем: менеджер вызывает это в фоне постоянно.
pub async fn find_cube(
    adapter: &Adapter,
    preferred: Option<&str>,
    timeout: Duration,
) -> Result<Peripheral, String> {
    // Уже известный системе кубик (видели в прошлом скане) — без ожидания рекламы.
    for p in adapter.peripherals().await.unwrap_or_default() {
        if preferred.is_some_and(|pref| pref == p.id().to_string()) {
            if let Ok(Some(props)) = p.properties().await {
                if props.rssi.is_some() {
                    return Ok(p);
                }
            }
        }
    }

    adapter.start_scan(ScanFilter::default()).await.map_err(|e| e.to_string())?;
    let mut events = adapter.events().await.map_err(|e| e.to_string())?;
    let deadline = tokio::time::Instant::now() + timeout;
    let found = loop {
        let ev = tokio::time::timeout_at(deadline, events.next()).await;
        let Ok(Some(ev)) = ev else { break None };
        let id = match ev {
            CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => id,
            CentralEvent::ServicesAdvertisement { id, .. } => id,
            _ => continue,
        };
        let Ok(p) = adapter.peripheral(&id).await else { continue };
        if preferred.is_some_and(|pref| pref == p.id().to_string()) {
            break Some(p);
        }
        if let Ok(Some(props)) = p.properties().await {
            // Живой кубик рекламируется как "deep.r" / "deep.n" (+ сервис 0x00FF), без наших UUID.
            let by_name = props.local_name.as_deref().is_some_and(|n| n.to_lowercase().starts_with("deep"));
            if by_name || is_cube_adv(&props.services) {
                break Some(p);
            }
        }
    };
    let _ = adapter.stop_scan().await;
    found.ok_or_else(|| "Кубик не найден рядом. Если он подключён к телефону — закройте там deep up.".into())
}

fn chr(p: &Peripheral, uuid: Uuid) -> Option<Characteristic> {
    p.characteristics().into_iter().find(|c| c.uuid == uuid)
}

pub async fn ensure_connected(p: &Peripheral) -> Result<(), String> {
    if !p.is_connected().await.unwrap_or(false) {
        tokio::time::timeout(Duration::from_secs(10), p.connect())
            .await
            .map_err(|_| "Таймаут подключения".to_string())?
            .map_err(|e| e.to_string())?;
    }
    if chr(p, CHR_CONTROL).is_none() {
        p.discover_services().await.map_err(|e| e.to_string())?;
    }
    if chr(p, CHR_CONTROL).is_none() {
        return Err("У устройства нет характеристики управления — это не кубик".into());
    }
    Ok(())
}

async fn read_string(p: &Peripheral, uuid: Uuid) -> Option<String> {
    let c = chr(p, uuid)?;
    let v = p.read(&c).await.ok()?;
    Some(String::from_utf8_lossy(&v).trim_end_matches('\0').to_string())
}

pub async fn device_info(p: &Peripheral) -> DeviceInfo {
    let name = p.properties().await.ok().flatten().and_then(|x| x.local_name);
    DeviceInfo {
        name,
        model: read_string(p, CHR_MODEL).await,
        firmware: read_string(p, CHR_FIRMWARE).await,
    }
}

pub async fn battery(p: &Peripheral) -> Battery {
    let level = match chr(p, CHR_BATTERY_LEVEL) {
        Some(c) => p.read(&c).await.ok().and_then(|v| v.first().copied()),
        None => None,
    };
    // 2A1A: 0x30 — заряжается, 0x20 — нет, 0x2F — неизвестно
    let charging = match chr(p, CHR_BATTERY_STATE) {
        Some(c) => p.read(&c).await.ok().and_then(|v| v.first().copied()).and_then(|b| match b {
            0x30 => Some(true),
            0x20 => Some(false),
            _ => None,
        }),
        None => None,
    };
    Battery { level, charging }
}

pub async fn read_state(p: &Peripheral) -> Result<ProgramState, String> {
    let c = chr(p, CHR_STATE).ok_or("нет характеристики состояния")?;
    let v = p.read(&c).await.map_err(|e| e.to_string())?;
    Ok(parse_state(&v))
}

pub async fn subscribe_state(p: &Peripheral) -> Result<(), String> {
    let c = chr(p, CHR_STATE).ok_or("нет характеристики состояния")?;
    p.subscribe(&c).await.map_err(|e| e.to_string())
}

pub async fn write_control(p: &Peripheral, packet: [u8; 8]) -> Result<(), String> {
    let c = chr(p, CHR_CONTROL).ok_or("нет характеристики управления")?;
    p.write(&c, &packet, WriteType::WithResponse).await.map_err(|e| e.to_string())
}
