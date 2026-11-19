use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

const LIMIT: usize = 1024 * 1024;
static WRITER: Mutex<()> = Mutex::new(());
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

fn read(dir: &Path) -> io::Result<Map<String, Value>> {
    let file = match File::open(dir.join("settings.json")) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.take((LIMIT + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn auto_lookup(dir: &Path) -> Option<bool> {
    read(dir).ok()?.get("autoLookup")?.as_bool()
}

pub fn offsets(dir: &Path) -> BTreeMap<String, [f32; 2]> {
    read(dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, value)| {
            if key.len() != 16 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let point: [f32; 2] = serde_json::from_value(value).ok()?;
            point.iter().all(|n| n.is_finite()).then_some((key, point))
        })
        .collect()
}

pub fn set_auto_lookup(dir: &Path, enabled: bool) -> io::Result<()> {
    update(dir, "autoLookup", Some(Value::Bool(enabled)))
}

pub fn set_offset(dir: &Path, key: &str, point: Option<[f32; 2]>) -> io::Result<()> {
    update(dir, key, point.map(Value::from))
}

fn update(dir: &Path, key: &str, value: Option<Value>) -> io::Result<()> {
    let _guard = WRITER.lock().unwrap();
    fs::create_dir_all(dir)?;
    let mut fields = read(dir)?;
    if let Some(value) = value {
        fields.insert(key.to_owned(), value);
    } else {
        fields.remove(key);
    }
    let bytes = serde_json::to_vec(&fields)?;
    let (temp, mut file) = loop {
        let serial = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!("settings-{}-{serial}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let written = file.write_all(&bytes).and_then(|_| file.sync_all());
    drop(file);
    let result = written.and_then(|_| fs::rename(&temp, dir.join("settings.json")));
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_and_old_offsets_survive_independent_updates_and_reset() {
        let dir =
            std::env::temp_dir().join(format!("rain-night-settings-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.json");
        let first = "1234567890abcdef";
        let second = "abcdef1234567890";
        fs::write(&file, br#"{"1234567890abcdef":[2,-3],"future":"preserve"}"#).unwrap();
        assert_eq!(auto_lookup(&dir), None);
        assert_eq!(offsets(&dir)[first], [2., -3.]);
        std::thread::scope(|scope| {
            scope.spawn(|| set_auto_lookup(&dir, true).unwrap());
            scope.spawn(|| set_offset(&dir, second, Some([8., 9.])).unwrap());
        });
        assert_eq!(auto_lookup(&dir), Some(true));
        assert_eq!(offsets(&dir)[second], [8., 9.]);
        set_auto_lookup(&dir, false).unwrap();
        set_offset(&dir, first, None).unwrap();
        assert_eq!(auto_lookup(&dir), Some(false));
        assert_eq!(read(&dir).unwrap()["future"], "preserve");
        assert_eq!(offsets(&dir).len(), 1);
        fs::remove_file(file).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
