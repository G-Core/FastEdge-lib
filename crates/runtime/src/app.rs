use chrono::{DateTime, Utc};
use serde::de::Visitor;
use serde::{Deserialize, Deserializer};
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
    pub secrets: Vec<Secret>,
}

pub type SecretValues = Vec<SecretValue>;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SecretValue {
    pub effective_from: u64,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Secret {
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

impl Default for Status {
    fn default() -> Self {
        Self::Enabled
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
            secrets: vec![Secret {
                name: "SECRET".to_smolstr(),
                secret_values: vec![SecretValue {
                    effective_from: 0,
                    value: "encrypted".to_string(),
                }],
            }],
        };

        assert_eq!(expected, assert_ok!(serde_json::from_str(&json)));
    }
}
