#[cfg(feature = "stats")]
use clickhouse::Row;
#[cfg(feature = "stats")]
use serde::Serialize;

#[cfg(feature = "stats")]
#[derive(Row, Debug, Serialize, Default)]
pub struct StatRow {
    pub app_id: u64,
    pub client_id: u64,
    pub timestamp: u32,
    pub app_name: String,
    pub status_code: u32,
    pub fail_reason: u32,
    pub billing_plan: String,
    pub time_elapsed: u64,
    pub memory_used: u64,
    pub pop: String,
    pub region: String
}

#[cfg(not(feature = "stats"))]
pub struct StatRow;

pub trait StatsWriter {
    fn write_stats(&self, stat: StatRow) -> impl std::future::Future<Output = ()> + Send;
}
