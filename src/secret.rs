use runtime::SecretValue;
use runtime::app::SecretOption;
use secret::SecretStrategy;
use smol_str::SmolStr;
use std::collections::{BTreeMap, HashMap};
use std::time::SystemTime;

/// Secrets of fastedge-run. Each name holds one or more slots, highest first. As on the edge, a
/// lookup at `at` returns the highest slot not above `at` (none when every slot is above it), and
/// `get` looks up at the current Unix time.
#[derive(Clone, Default)]
pub struct SecretImpl {
    inner: HashMap<String, Vec<SecretValue>>,
}

impl SecretImpl {
    pub fn insert(&mut self, name: String, mut values: Vec<SecretValue>) {
        values.sort_by_key(|slot| std::cmp::Reverse(slot.effective_from));
        self.inner.insert(name, values);
    }
}

impl SecretStrategy for SecretImpl {
    fn get(&self, key: String) -> anyhow::Result<Option<Vec<u8>>> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        self.get_effective_at(key, now)
    }

    fn get_effective_at(&self, key: String, at: u64) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self
            .inner
            .get(&key)
            .and_then(|slots| slots.iter().find(|slot| at >= slot.effective_from))
            .map(|slot| slot.value.as_bytes().to_vec()))
    }
}

/// Secrets from `--secret` and `.env.secrets`: `NAME@SLOT=value` is slot SLOT (a positive integer,
/// in plain digits) of secret NAME; a bare `NAME=value` is slot 0. Each slot has one spelling, so
/// the per-key precedence of the merge (CLI over dotenv) is per-slot precedence.
pub fn secret_options(args: HashMap<SmolStr, SmolStr>) -> anyhow::Result<Vec<SecretOption>> {
    let mut secrets: BTreeMap<SmolStr, Vec<SecretValue>> = BTreeMap::new();
    for (key, value) in args {
        let (name, slot) = match key.split_once('@') {
            None => (key.as_str(), 0),
            Some((name, raw)) => {
                let slot = raw
                    .parse::<u64>()
                    .ok()
                    .filter(|slot| slot.to_string() == raw);
                let Some(slot) = slot.filter(|&slot| slot > 0) else {
                    anyhow::bail!(
                        "secret {key}: write slot 0 as a bare `{name}`, and other slots as `{name}@N` with N a positive integer"
                    );
                };
                (name, slot)
            }
        };
        anyhow::ensure!(!name.is_empty(), "secret {key}: no name before '@'");
        secrets
            .entry(SmolStr::new(name))
            .or_default()
            .push(SecretValue {
                effective_from: slot,
                value: value.to_string(),
            });
    }
    Ok(secrets
        .into_iter()
        .map(|(name, secret_values)| SecretOption {
            name,
            secret_values,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> HashMap<SmolStr, SmolStr> {
        pairs
            .iter()
            .map(|(k, v)| (SmolStr::new(k), SmolStr::new(v)))
            .collect()
    }

    fn store(pairs: &[(&str, &str)]) -> SecretImpl {
        let mut store = SecretImpl::default();
        for s in secret_options(args(pairs)).unwrap() {
            store.insert(s.name.to_string(), s.secret_values);
        }
        store
    }

    fn at(store: &SecretImpl, key: &str, at: u64) -> Option<String> {
        store
            .get_effective_at(key.into(), at)
            .unwrap()
            .map(|v| String::from_utf8(v).unwrap())
    }

    #[test]
    fn a_bare_name_is_slot_zero() {
        let store = store(&[("TOKEN", "only")]);
        assert_eq!(at(&store, "TOKEN", 0).as_deref(), Some("only"));
        assert_eq!(
            at(&store, "TOKEN", u64::from(u32::MAX)).as_deref(),
            Some("only")
        );
        assert_eq!(store.get("TOKEN".into()).unwrap(), Some(b"only".to_vec()));
    }

    #[test]
    fn the_highest_slot_not_above_at_wins() {
        let store = store(&[("TOKEN", "original"), ("TOKEN@5", "updated")]);
        assert_eq!(at(&store, "TOKEN", 4).as_deref(), Some("original"));
        assert_eq!(at(&store, "TOKEN", 5).as_deref(), Some("updated"));
        assert_eq!(at(&store, "TOKEN", 1_000).as_deref(), Some("updated"));
        assert_eq!(at(&store, "MISSING", 5), None);
    }

    #[test]
    fn nothing_below_the_lowest_slot() {
        let store = store(&[("TOKEN@5", "updated")]);
        assert_eq!(at(&store, "TOKEN", 3), None);
    }

    #[test]
    fn get_is_the_slot_effective_now() {
        // A slot dated far in the future is not effective yet.
        let store = store(&[("TOKEN", "current"), ("TOKEN@4000000000", "future")]);
        assert_eq!(
            store.get("TOKEN".into()).unwrap(),
            Some(b"current".to_vec())
        );
    }

    #[test]
    fn a_bad_slot_is_an_error() {
        // One spelling per slot: slot 0 is the bare name, others plain positive digits.
        for key in [
            "TOKEN@abc",
            "TOKEN@-1",
            "TOKEN@",
            "@5",
            "TOKEN@0",
            "TOKEN@05",
            "TOKEN@+5",
        ] {
            assert!(secret_options(args(&[(key, "v")])).is_err(), "{key}");
        }
    }
}
