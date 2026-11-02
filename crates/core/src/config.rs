//! Tiny `key = value` config parser with CLI overrides (`--key value` / `--flag`).

use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct Config {
    map: HashMap<String, String>,
}

impl Config {
    pub fn parse(text: &str) -> Config {
        let mut map = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let v = v.trim();
                let v = v
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .unwrap_or(v);
                map.insert(normalize(k), v.to_string());
            }
        }
        Config { map }
    }

    pub fn load(path: &Path) -> std::io::Result<Config> {
        Ok(Self::parse(&std::fs::read_to_string(path)?))
    }

    /// Apply `--key value` and `--flag` style args. Returns positional args.
    pub fn apply_args(&mut self, args: &[String]) -> Vec<String> {
        let mut pos = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(k) = a.strip_prefix("--") {
                if let Some((k, v)) = k.split_once('=') {
                    self.map.insert(normalize(k), v.to_string());
                } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                    self.map.insert(normalize(k), args[i + 1].clone());
                    i += 1;
                } else {
                    self.map.insert(normalize(k), "true".into());
                }
            } else {
                pos.push(a.clone());
            }
            i += 1;
        }
        pos
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.map
            .get(k)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
    }

    pub fn str_or(&self, k: &str, d: &str) -> String {
        self.get(k).unwrap_or(d).to_string()
    }

    pub fn num<T: std::str::FromStr>(&self, k: &str) -> Result<Option<T>, String> {
        match self.get(k) {
            None => Ok(None),
            Some(v) => v
                .parse::<T>()
                .map(Some)
                .map_err(|_| format!("invalid value for '{k}': {v}")),
        }
    }

    pub fn flag(&self, k: &str) -> bool {
        matches!(self.get(k), Some("true" | "1" | "yes" | "on"))
    }
}

fn normalize(k: &str) -> String {
    k.trim().to_ascii_lowercase().replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_override() {
        let mut c = Config::parse("# c\nport = 1234\npassphrase = \"a b\"\nside=right\n");
        assert_eq!(c.get("port"), Some("1234"));
        assert_eq!(c.get("passphrase"), Some("a b"));
        let pos = c.apply_args(&["x".into(), "--port".into(), "99".into(), "--dry-run".into()]);
        assert_eq!(c.num::<u16>("port").unwrap(), Some(99));
        assert!(c.flag("dry_run"));
        assert_eq!(pos, vec!["x".to_string()]);
    }
}
