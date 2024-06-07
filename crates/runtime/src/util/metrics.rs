use lazy_static::lazy_static;
use prometheus::{self, opts, register_int_counter, IntCounter};

use crate::AppResult;

lazy_static! {
    static ref TOTAL_COUNT: IntCounter =
        register_int_counter!("fastedge_call_count", "Total number of app calls.").unwrap();
}

lazy_static! {
    static ref ERROR_COUNT: IntCounter = register_int_counter!(opts!(
        "fastedge_error_total_count",
        "Number of failed app calls."
    ))
    .unwrap();
}
lazy_static! {
    static ref UNKNOWN_COUNT: IntCounter = register_int_counter!(opts!(
        "fastedge_error_unknown_count",
        "Number of calls for unknown app."
    ))
    .unwrap();
}
lazy_static! {
    static ref TIMEOUT_COUNT: IntCounter =
        register_int_counter!(opts!("fastedge_error_timeout_count", "Number of timeouts."))
            .unwrap();
}
lazy_static! {
    static ref OOM_COUNT: IntCounter =
        register_int_counter!(opts!("fastedge_error_oom_count", "Number of OOMs.")).unwrap();
}
lazy_static! {
    static ref OTHER_ERROR_COUNT: IntCounter = register_int_counter!(opts!(
        "fastedge_error_other_count",
        "Number of other error."
    ))
    .unwrap();
}

pub fn metrics(result: AppResult) {
    TOTAL_COUNT.inc();
    if result != AppResult::SUCCESS {
        ERROR_COUNT.inc();
    }
    match result {
        AppResult::UNKNOWN => UNKNOWN_COUNT.inc(),
        AppResult::TIMEOUT => TIMEOUT_COUNT.inc(),
        AppResult::OOM => OOM_COUNT.inc(),
        AppResult::OTHER => OTHER_ERROR_COUNT.inc(),
        _ => {}
    };
}
