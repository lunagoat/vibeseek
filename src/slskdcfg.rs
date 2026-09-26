//! Reading/editing slskd.yml. slskd watches the file and hot-reloads most settings
//! (including `soulseek.listen_port` and upload groups / blacklist).

use anyhow::{bail, Context, Result};
use serde_yaml::{Mapping, Value};
use std::path::Path;

fn load(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_yaml::from_str(&text)?)
}

fn save(path: &Path, v: &Value) -> Result<()> {
    let text = format!("# Managed by vibeseek (edit freely; slskd reloads on change)\n{}", serde_yaml::to_string(v)?);
    // Write in place: slskd's file watcher reacts to modifications but not to a file being
    // replaced by rename, so an atomic temp-file swap would never be picked up.
    std::fs::write(path, text)?;
    Ok(())
}

fn get<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().try_fold(v, |cur, k| cur.get(*k))
}

/// Walk/create nested mappings.
fn get_mut<'a>(v: &'a mut Value, keys: &[&str]) -> Result<&'a mut Value> {
    let mut cur = v;
    for k in keys {
        if cur.is_null() {
            *cur = Value::Mapping(Mapping::new());
        }
        let Value::Mapping(m) = cur else { bail!("slskd.yml: '{k}' parent is not a mapping") };
        cur = m.entry(Value::String(k.to_string())).or_insert(Value::Null);
    }
    Ok(cur)
}

pub fn read_api_key(path: &Path) -> Result<String> {
    let v = load(path)?;
    let keys = get(&v, &["web", "authentication", "api_keys"]).context("no web.authentication.api_keys in slskd.yml")?;
    if let Some(k) = keys.get("vibeseek").and_then(|e| e.get("key")).and_then(|k| k.as_str()) {
        return Ok(k.to_string());
    }
    keys.as_mapping()
        .and_then(|m| m.values().find_map(|e| e.get("key").and_then(|k| k.as_str())))
        .map(str::to_string)
        .context("no API key found in slskd.yml")
}

pub fn listen_port(path: &Path) -> Result<u16> {
    let v = load(path)?;
    Ok(get(&v, &["soulseek", "listen_port"]).and_then(|p| p.as_u64()).unwrap_or(50300) as u16)
}

pub fn set_listen_port(path: &Path, port: u16) -> Result<()> {
    let mut v = load(path)?;
    *get_mut(&mut v, &["soulseek", "listen_port"])? = Value::Number(port.into());
    save(path, &v)
}

fn members_mut(v: &mut Value) -> Result<&mut Vec<Value>> {
    let m = get_mut(v, &["transfers", "groups", "blacklisted", "members"])?;
    if !m.is_sequence() {
        *m = Value::Sequence(vec![]);
    }
    Ok(m.as_sequence_mut().unwrap())
}

pub fn banned(path: &Path) -> Result<Vec<String>> {
    let v = load(path)?;
    Ok(get(&v, &["transfers", "groups", "blacklisted", "members"])
        .and_then(|m| m.as_sequence())
        .map(|s| s.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default())
}

/// Returns false if the user was already banned.
pub fn ban(path: &Path, user: &str) -> Result<bool> {
    let mut v = load(path)?;
    let members = members_mut(&mut v)?;
    if members.iter().any(|m| m.as_str() == Some(user)) {
        return Ok(false);
    }
    members.push(Value::String(user.to_string()));
    save(path, &v)?;
    Ok(true)
}

/// Returns false if the user wasn't banned.
pub fn unban(path: &Path, user: &str) -> Result<bool> {
    let mut v = load(path)?;
    let members = members_mut(&mut v)?;
    let before = members.len();
    members.retain(|m| m.as_str() != Some(user));
    let changed = members.len() != before;
    if changed {
        save(path, &v)?;
    }
    Ok(changed)
}
