use crate::executor::AppResult;
use http::{header::CONTENT_TYPE, Request, Response};
use hyper::Body;
use once_cell::sync::Lazy;
use prometheus::{
    self, opts, register_int_counter, register_int_gauge, Encoder, IntCounter, IntGauge,
    TextEncoder,
};
use std::time::SystemTime;

static TOTAL_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(opts!("fastedge_call_count", "Total number of app calls.")).unwrap()
});

static ERROR_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(opts!(
        "fastedge_error_total_count",
        "Number of failed app calls."
    ))
    .unwrap()
});

static UNKNOWN_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(opts!(
        "fastedge_error_unknown_count",
        "Number of calls for unknown app."
    ))
    .unwrap()
});

static TIMEOUT_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(opts!("fastedge_error_timeout_count", "Number of timeouts.")).unwrap()
});

static OOM_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(opts!("fastedge_error_oom_count", "Number of OOMs.")).unwrap()
});

static OTHER_ERROR_COUNT: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(opts!(
        "fastedge_error_other_count",
        "Number of other error."
    ))
    .unwrap()
});

static KNOWN_APP_COUNT: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(opts!(
        "fastedge_known_apps_count",
        "Number of configured apps."
    ))
    .unwrap()
});

static LAST_UPDATE_TIMESTAMP: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(opts!(
        "fastedge_last_update_ts",
        "UNIX time of last config update."
    ))
    .unwrap()
});

pub async fn serve_req(_req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
    let mut buffer = Vec::new();
    let encoder = TextEncoder::new();

    let metric_families = prometheus::gather();
    encoder.encode(&metric_families, &mut buffer).unwrap();

    let response = Response::builder()
        .status(200)
        .header(CONTENT_TYPE, encoder.format_type())
        .body(Body::from(buffer))
        .unwrap();

    Ok(response)
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

pub fn config_update(app_count: usize) {
    KNOWN_APP_COUNT.set(app_count.try_into().unwrap());
    LAST_UPDATE_TIMESTAMP.set(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .try_into()
            .unwrap(),
    );
}
