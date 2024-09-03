use lazy_static::lazy_static;
use prometheus::{self, register_int_counter_vec, register_histogram_vec, IntCounterVec, HistogramVec};

use crate::AppResult;

lazy_static! {
    static ref TOTAL_COUNT: IntCounterVec =
        register_int_counter_vec!("fastedge_call_count", "Total number of app calls.", &["executor"]).unwrap();

    static ref ERROR_COUNT: IntCounterVec = register_int_counter_vec!(
        "fastedge_error_total_count",
        "Number of failed app calls.", &["executor", "reason"]
    )
    .unwrap();

    static ref REQUEST_DURATION: HistogramVec = register_histogram_vec!("fastedge_request_duration", "Request duration", &["executor"]).unwrap();
    static ref MEMORY_USAGE: HistogramVec = register_histogram_vec!("fastedge_wasm_memory_used", "WASM Memory usage", &["executor"]).unwrap();
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
        REQUEST_DURATION.with_label_values(label).observe(duration as f64);
    }
    if let Some(memory_used) = memory_used {
        MEMORY_USAGE.with_label_values(label).observe(memory_used as f64);
    }
}
