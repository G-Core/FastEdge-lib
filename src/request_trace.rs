//! The current request's `traceparent`, for the stores service's storage trace.
//!
//! http-service runs each request inside an `http` span with a `traceparent` field (the
//! client's header, or a generated one) and carries that span into the task that runs the
//! guest. [`TraceparentLayer`] keeps the field on the span; [`current`] reads it back from
//! inside a host call, so storage calls can be tagged with the request that made them.

use std::fmt::Debug;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

struct Traceparent(String);

pub struct TraceparentLayer;

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for TraceparentLayer {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if attrs.metadata().name() != "http" {
            return;
        }
        let mut visitor = Visitor(None);
        attrs.record(&mut visitor);
        if let (Some(value), Some(span)) = (visitor.0, ctx.span(id)) {
            span.extensions_mut().insert(Traceparent(value));
        }
    }
}

struct Visitor(Option<String>);

impl Visit for Visitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "traceparent" {
            self.0 = Some(value.to_string());
        }
    }

    // `traceparent = %value` arrives as a Display value, whose Debug is its Display.
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "traceparent" {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

/// The `traceparent` of the request being handled on this task, if any.
pub fn current() -> Option<String> {
    let id = tracing::Span::current().id()?;
    tracing::dispatcher::get_default(|dispatch| {
        let registry = dispatch.downcast_ref::<Registry>()?;
        let span = registry.span(&id)?;
        span.scope()
            .find_map(|s| s.extensions().get::<Traceparent>().map(|t| t.0.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn reads_the_traceparent_of_the_enclosing_http_span() {
        let subscriber = Registry::default().with(TraceparentLayer);
        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(current(), None);
            let tp = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
            let span = tracing::error_span!("http", app = "a", traceparent = %tp);
            let _http = span.enter();
            let inner = tracing::info_span!("inner");
            let _inner = inner.enter();
            assert_eq!(current().as_deref(), Some(tp));
        });
    }
}
