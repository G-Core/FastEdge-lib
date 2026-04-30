use chrono::{DateTime, Utc};
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use smol_str::SmolStr;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Formatter;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct App {
    pub binary_id: u64,
    pub max_duration: u64,
    pub mem_limit: usize,
    #[serde(default)]
    pub env: HashMap<SmolStr, SmolStr>,
    #[serde(default)]
    pub rsp_headers: HashMap<SmolStr, SmolStr>,
    #[serde(default)]
    pub log: Log,
    #[serde(default)]
    pub app_id: u64,
    pub client_id: u64,
    pub plan: SmolStr,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub debug_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub secrets: Vec<SecretOption>,
    #[serde(default)]
    pub kv_stores: Vec<KvStoreOption>,
    #[serde(default)]
    pub plan_id: u64,
    #[serde(default)]
    pub cache_mode: CacheMode,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct KvStoreOption {
    /// The url
    #[serde(default)]
    pub param: SmolStr,
    #[serde(default)]
    pub name: SmolStr,
    #[serde(default)]
    pub prefix: SmolStr,
    #[serde(default = "KvStoreOption::default_cache_size")]
    pub cache_size: u64,
    #[serde(default = "KvStoreOption::default_cache_ttl")]
    pub cache_ttl: u64,
}

impl KvStoreOption {
    fn default_cache_size() -> u64 {
        1000
    }

    fn default_cache_ttl() -> u64 {
        60
    }
}

pub type SecretValues = Vec<SecretValue>;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SecretValue {
    pub effective_from: u64,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SecretOption {
    pub name: SmolStr,
    pub secret_values: SecretValues,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Log {
    #[default]
    None,
    #[cfg(feature = "kafka_log")]
    Kafka,
    #[cfg(feature = "victoria_log")]
    Victoria,
}

#[derive(Debug, Clone, PartialEq)]
#[repr(u8)]
pub enum Status {
    Draft = 0,
    Enabled = 1,
    Disabled = 2,
    RateLimited = 3,
    Suspended = 5,
}

#[derive(Debug, Clone, PartialEq)]
#[repr(u8)]
pub enum CacheMode {
    Disabled = 0,
    Enabled = 1,
}

impl Default for Status {
    fn default() -> Self {
        Self::Enabled
    }
}

impl Default for CacheMode {
    fn default() -> Self {
        Self::Disabled
    }
}

struct StatusVisitor;

impl<'de> Visitor<'de> for StatusVisitor {
    type Value = Status;

    fn expecting(&self, formatter: &mut Formatter) -> fmt::Result {
        formatter.write_str("unsigned integer value")
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        match v {
            0 => Ok(Status::Draft),
            1 => Ok(Status::Enabled),
            2 => Ok(Status::Disabled),
            3 | 4 => Ok(Status::RateLimited),
            5 => Ok(Status::Suspended),
            _ => Err(E::custom("status not in range: [0..5]")),
        }
    }
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D>(deserializer: D) -> anyhow::Result<Status, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_u64(StatusVisitor)
    }
}

struct CacheModeVisitor;

impl<'de> Visitor<'de> for CacheModeVisitor {
    type Value = CacheMode;

    fn expecting(&self, formatter: &mut Formatter) -> fmt::Result {
        formatter.write_str("unsigned integer value (0 = disabled, 1 = enabled)")
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        match v {
            0 => Ok(CacheMode::Disabled),
            1 => Ok(CacheMode::Enabled),
            _ => Err(E::custom("cache_mode not in range: [0..1]")),
        }
    }
}

impl<'de> Deserialize<'de> for CacheMode {
    fn deserialize<D>(deserializer: D) -> Result<CacheMode, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_u64(CacheModeVisitor)
    }
}

impl Serialize for CacheMode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(match self {
            CacheMode::Disabled => 0,
            CacheMode::Enabled => 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claims::{assert_err, assert_ok};
    use serde_json::json;
    use smol_str::ToSmolStr;

    #[test]
    fn test_status_deserialize() {
        assert_eq!(Status::Draft, assert_ok!(serde_json::from_str("0")));
        assert_eq!(Status::Enabled, assert_ok!(serde_json::from_str("1")));
        assert_eq!(Status::Disabled, assert_ok!(serde_json::from_str("2")));
        assert_eq!(Status::RateLimited, assert_ok!(serde_json::from_str("3")));
        assert_eq!(Status::RateLimited, assert_ok!(serde_json::from_str("4")));
        assert_eq!(Status::Suspended, assert_ok!(serde_json::from_str("5")));
        assert_err!(serde_json::from_str::<Status>("6"));
    }

    #[test]
    fn deserialize_app() {
        let json = json!({
            "binary_id": 110,
            "max_duration": 10,
            "mem_limit": 1000000,
            "rsp_headers": {"RES_HEADER_03": "03"},
            "app_id": 12345,
            "client_id": 23456,
            "plan": "test_plan",
            "plan_id": 0,
            "status": 1,
            "debug_until": "2037-01-01T12:00:27.87Z",
            "secrets":[{"name":"SECRET","secret_values":[{"effective_from":0,"value":"encrypted"}]}]

        });
        let json = assert_ok!(serde_json::to_string_pretty(&json));

        let expected = App {
            binary_id: 110,
            max_duration: 10,
            mem_limit: 1000000,
            env: Default::default(),
            rsp_headers: HashMap::from([("RES_HEADER_03".to_smolstr(), "03".to_smolstr())]),
            log: Default::default(),
            app_id: 12345,
            client_id: 23456,
            plan: "test_plan".to_smolstr(),
            status: Status::Enabled,
            debug_until: Some(assert_ok!("2037-01-01T12:00:27.87Z".parse())),
            secrets: vec![SecretOption {
                name: "SECRET".to_smolstr(),
                secret_values: vec![SecretValue {
                    effective_from: 0,
                    value: "encrypted".to_string(),
                }],
            }],
            kv_stores: vec![],
            plan_id: 0,
            cache_mode: CacheMode::Disabled,
        };

        assert_eq!(expected, assert_ok!(serde_json::from_str(&json)));
    }

    #[test]
    fn test_log_deserialize_default() {
        let log: Log = assert_ok!(serde_json::from_str("\"none\""));
        assert_eq!(log, Log::None);
    }

    #[test]
    fn test_log_deserialize_missing_defaults_to_none() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            log: Log,
        }
        let w: Wrapper = assert_ok!(serde_json::from_str("{}"));
        assert_eq!(w.log, Log::None);
    }

    #[cfg(feature = "kafka_log")]
    #[test]
    fn test_log_deserialize_kafka() {
        let log: Log = assert_ok!(serde_json::from_str("\"kafka\""));
        assert_eq!(log, Log::Kafka);
    }

    #[cfg(feature = "victoria_log")]
    #[test]
    fn test_log_deserialize_victoria_logs() {
        let log: Log = assert_ok!(serde_json::from_str("\"victoria\""));
        assert_eq!(log, Log::Victoria);
    }

    #[cfg(feature = "victoria_log")]
    #[test]
    fn deserialize_app_with_victoria_log() {
        let json = json!({
            "binary_id": 1,
            "max_duration": 5,
            "mem_limit": 512000,
            "app_id": 1,
            "client_id": 2,
            "plan": "basic",
            "plan_id": 0,
            "status": 1,
            "log": "victoria"
        });
        let json = assert_ok!(serde_json::to_string(&json));
        let app: App = assert_ok!(serde_json::from_str(&json));
        assert_eq!(app.log, Log::Victoria);
    }

    #[test]
    fn test_log_deserialize_invalid() {
        assert_err!(serde_json::from_str::<Log>("\"unknown\""));
    }

    #[test]
    fn test_kv_store_option_deserialize_defaults() {
        let json = r#"{
        "name": "store",
        "prefix": "pre"
    }"#;
        let kv: KvStoreOption = serde_json::from_str(json).unwrap();
        assert_eq!(kv.param, "");
        assert_eq!(kv.name, "store");
        assert_eq!(kv.prefix, "pre");
        assert_eq!(kv.cache_size, 1000);
        assert_eq!(kv.cache_ttl, 60);
    }

    #[test]
    fn test_kv_store_option_deserialize_custom() {
        let json = r#"{
        "param": "url2",
        "name": "store2",
        "prefix": "pre2",
        "cache_size": 5000,
        "cache_ttl": 120
    }"#;
        let kv: KvStoreOption = serde_json::from_str(json).unwrap();
        assert_eq!(kv.param, "url2");
        assert_eq!(kv.name, "store2");
        assert_eq!(kv.prefix, "pre2");
        assert_eq!(kv.cache_size, 5000);
        assert_eq!(kv.cache_ttl, 120);
    }

    #[test]
    fn test_cache_mode_deserialize_disabled() {
        assert_eq!(
            CacheMode::Disabled,
            assert_ok!(serde_json::from_str::<CacheMode>("0"))
        );
    }

    #[test]
    fn test_cache_mode_deserialize_enabled() {
        assert_eq!(
            CacheMode::Enabled,
            assert_ok!(serde_json::from_str::<CacheMode>("1"))
        );
    }

    #[test]
    fn test_cache_mode_deserialize_out_of_range() {
        assert_err!(serde_json::from_str::<CacheMode>("2"));
        assert_err!(serde_json::from_str::<CacheMode>("255"));
    }

    #[test]
    fn test_cache_mode_deserialize_invalid_type() {
        // Strings are not accepted - only unsigned integers
        assert_err!(serde_json::from_str::<CacheMode>("\"enabled\""));
        assert_err!(serde_json::from_str::<CacheMode>("true"));
    }

    #[test]
    fn test_cache_mode_default_is_disabled() {
        assert_eq!(CacheMode::default(), CacheMode::Disabled);
    }

    #[test]
    fn test_cache_mode_serialize() {
        assert_eq!("0", assert_ok!(serde_json::to_string(&CacheMode::Disabled)));
        assert_eq!("1", assert_ok!(serde_json::to_string(&CacheMode::Enabled)));
    }

    #[test]
    fn test_cache_mode_roundtrip() {
        for mode in [CacheMode::Disabled, CacheMode::Enabled] {
            let serialized = assert_ok!(serde_json::to_string(&mode));
            let deserialized: CacheMode = assert_ok!(serde_json::from_str(&serialized));
            assert_eq!(mode, deserialized);
        }
    }

    #[test]
    fn test_cache_mode_default_when_missing() {
        // Verify `#[serde(default)]` on App.cache_mode works
        let json = json!({
            "binary_id": 1,
            "max_duration": 5,
            "mem_limit": 512000,
            "app_id": 1,
            "client_id": 2,
            "plan": "basic",
        });
        let json = assert_ok!(serde_json::to_string(&json));
        let app: App = assert_ok!(serde_json::from_str(&json));
        assert_eq!(app.cache_mode, CacheMode::Disabled);
    }

    #[test]
    fn test_cache_mode_explicit_enabled_in_app() {
        let json = json!({
            "binary_id": 1,
            "max_duration": 5,
            "mem_limit": 512000,
            "app_id": 1,
            "client_id": 2,
            "plan": "basic",
            "cache_mode": 1,
        });
        let json = assert_ok!(serde_json::to_string(&json));
        let app: App = assert_ok!(serde_json::from_str(&json));
        assert_eq!(app.cache_mode, CacheMode::Enabled);
    }
}
