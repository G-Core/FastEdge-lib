use lazy_static::lazy_static;
use prometheus::{
    self, register_histogram_vec, register_int_counter_vec, HistogramVec, IntCounterVec,
};

use crate::AppResult;

lazy_static! {
    static ref TOTAL_COUNT: IntCounterVec = register_int_counter_vec!(
        "fastedge_call_count",
        "Total number of app calls.",
        &["executor"]
    )
    .unwrap();
    static ref ERROR_COUNT: IntCounterVec = register_int_counter_vec!(
        "fastedge_error_total_count",
        "Number of failed app calls.",
        &["executor", "reason"]
    )
    .unwrap();
    static ref REQUEST_DURATION: HistogramVec = register_histogram_vec!(
        "fastedge_request_duration",
        "Request duration",
        &["executor"],
        vec![
            0.0001, 0.0002, 0.0003, 0.0005, 0.001, 0.002, 0.003, 0.005, 0.01, 0.02, 0.03, 0.05,
            0.1, 0.2, 0.3, 0.5, 1.0, 10.0
        ]
    )
    .unwrap();
    static ref MEMORY_USAGE: IntCounterVec = register_int_counter_vec!(
        "fastedge_wasm_memory_used",
        "WASM Memory usage",
        &["executor"]
    )
    .unwrap();
}

pub fn metrics(result: AppResult, label: &[&str], duration: Option<u64>, memory_used: Option<u64>) {
    TOTAL_COUNT.with_label_values(label).inc();

    if result != AppResult::SUCCESS {
        let mut values: Vec<&str> = label.iter().map(|v| *v).collect();
        match result {
            AppResult::UNKNOWN => values.push("unknown"),
            AppResult::TIMEOUT => values.push("timeout"),
            AppResult::OOM => values.push("oom"),
            AppResult::OTHER => values.push("other"),
            _ => {}
        };

        ERROR_COUNT.with_label_values(values.as_slice()).inc();
    }

    if let Some(duration) = duration {
        REQUEST_DURATION
            .with_label_values(label)
            .observe((duration as f64) / 1_000_000.0);
    }
    if let Some(memory_used) = memory_used {
        MEMORY_USAGE.with_label_values(label).inc_by(memory_used);
    }
}
