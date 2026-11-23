//! Current weather from Open-Meteo (free, no API key). Location comes from
//! the configured city (geocoded) or, when empty, Windows location services.
//! Coordinates are rounded to ~1 km before they leave the PC.

use crate::config;
use crate::worker::Shared;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Devices::Geolocation::{GeolocationAccessStatus, Geolocator};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

#[derive(Clone, Debug)]
pub struct WeatherNow {
    pub temp: f32,
    pub code: i32,
    pub is_day: bool,
    pub max: f32,
    pub min: f32,
    pub place: String,
}

pub fn spawn(shared: Arc<Mutex<Shared>>) {
    std::thread::Builder::new()
        .name("notch-weather".into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let mut last: Option<config::WeatherCfg> = None;
            let mut next = Instant::now();
            loop {
                let cfg = config::current();
                if !cfg.features.weather {
                    shared.lock().unwrap().weather = None;
                    last = None;
                } else if last.as_ref() != Some(&cfg.weather) || Instant::now() >= next {
                    let res = fetch(&cfg.weather);
                    let mut sh = shared.lock().unwrap();
                    match res {
                        Ok(w) => {
                            sh.weather = Some(w);
                            sh.weather_err = None;
                            next = Instant::now() + Duration::from_secs(15 * 60);
                        }
                        Err(e) => {
                            sh.weather_err = Some(e);
                            next = Instant::now() + Duration::from_secs(3 * 60);
                        }
                    }
                    last = Some(cfg.weather.clone());
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        })
        .expect("spawn weather");
}

fn http_get(url: &str) -> Result<String, String> {
    crate::net::get(url).ok_or_else(|| "Tidak bisa terhubung ke server cuaca".to_string())
}

use crate::net::encode;
fn geocode(city: &str) -> Result<(f64, f64, String), String> {
    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=id&format=json",
        encode(city.trim())
    );
    let v: Value = serde_json::from_str(&http_get(&url)?).map_err(|e| e.to_string())?;
    let r = v["results"].get(0).ok_or_else(|| format!("Kota \"{city}\" tidak ditemukan"))?;
    let lat = r["latitude"].as_f64().ok_or("Data kota tidak valid")?;
    let lon = r["longitude"].as_f64().ok_or("Data kota tidak valid")?;
    let name = r["name"].as_str().unwrap_or(city).to_string();
    Ok((lat, lon, name))
}

fn locate() -> Result<(f64, f64, String), String> {
    let off = || "Lokasi Windows mati. Isi kota di pengaturan".to_string();
    let status = Geolocator::RequestAccessAsync()
        .and_then(|op| op.join())
        .map_err(|_| off())?;
    if status != GeolocationAccessStatus::Allowed {
        return Err(off());
    }
    let pos = Geolocator::new()
        .and_then(|g| g.GetGeopositionAsync())
        .and_then(|op| op.join())
        .and_then(|p| p.Coordinate())
        .and_then(|c| c.Point())
        .and_then(|p| p.Position())
        .map_err(|_| off())?;
    Ok((pos.Latitude, pos.Longitude, String::new()))
}

fn fetch(cfg: &config::WeatherCfg) -> Result<WeatherNow, String> {
    let (lat, lon, mut place) = if cfg.city.trim().is_empty() {
        locate()?
    } else {
        geocode(&cfg.city)?
    };
    let unit = if cfg.fahrenheit { "&temperature_unit=fahrenheit" } else { "" };
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat:.2}&longitude={lon:.2}\
         &current=temperature_2m,weather_code,is_day\
         &daily=temperature_2m_max,temperature_2m_min&timezone=auto&forecast_days=1{unit}"
    );
    let v: Value = serde_json::from_str(&http_get(&url)?).map_err(|e| e.to_string())?;
    let cur = &v["current"];
    if place.is_empty() {
        place = v["timezone"]
            .as_str()
            .and_then(|t| t.rsplit('/').next())
            .unwrap_or("")
            .replace('_', " ");
    }
    Ok(WeatherNow {
        temp: cur["temperature_2m"].as_f64().ok_or("Data cuaca tidak valid")? as f32,
        code: cur["weather_code"].as_i64().unwrap_or(0) as i32,
        is_day: cur["is_day"].as_i64().unwrap_or(1) == 1,
        max: v["daily"]["temperature_2m_max"][0].as_f64().unwrap_or(0.0) as f32,
        min: v["daily"]["temperature_2m_min"][0].as_f64().unwrap_or(0.0) as f32,
        place,
    })
}

/// Indonesian description for a WMO weather code.
pub fn describe(code: i32) -> &'static str {
    match code {
        0 => "Cerah",
        1 => "Cerah berawan",
        2 => "Berawan",
        3 => "Mendung",
        45 | 48 => "Berkabut",
        51..=57 => "Gerimis",
        61..=65 | 80..=82 => "Hujan",
        66 | 67 => "Hujan es",
        71..=77 | 85 | 86 => "Salju",
        95..=99 => "Badai petir",
        _ => "—",
    }
}
