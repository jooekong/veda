//! Prometheus text exposition → OTLP `ExportMetricsServiceRequest`.
//!
//! Full bridge: counters → `Sum` (monotonic, CUMULATIVE), gauges → `Gauge`,
//! histograms → `Histogram` (CUMULATIVE, cumulative buckets diffed into
//! per-bucket counts). Summaries are skipped (veda emits none).
//!
//! Values are written `as_double`: the Prometheus text has already collapsed
//! the source numeric type, so "pick int vs double by type" isn't recoverable —
//! a uniform double is the executable choice (plan §3.3), and the company
//! platform accepts it (verified on the MVP send).
//!
//! Each data point dual-writes its dimensions to `attributes` (KeyValue) AND
//! deprecated `labels` (StringKeyValue): the company Collector is an old
//! receiver that reads `labels`, so omitting them means no data shows up.

#![allow(deprecated)] // we dual-write the deprecated `labels` field on purpose.

use std::collections::HashMap;
use std::io;

use prometheus_parse::{Labels, Sample, Scrape, Value};

use super::proto::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use super::proto::opentelemetry::proto::common::v1::{
    any_value, AnyValue, InstrumentationLibrary, KeyValue, StringKeyValue,
};
use super::proto::opentelemetry::proto::metrics::v1::{
    metric, number_data_point, AggregationTemporality, Gauge, Histogram, HistogramDataPoint,
    InstrumentationLibraryMetrics, Metric, NumberDataPoint, ResourceMetrics, Sum,
};
use super::proto::opentelemetry::proto::resource::v1::Resource;

/// Internal-SDK instrumentation library name, per OTHER_LANGUAGE_SDK_INTEGRATION.md.
const INSTRUMENTATION_NAME: &str = "monitor";

/// Cumulative time semantics: `start` is the exporter's fixed start instant
/// (cumulative window origin); `now` is this collection's timestamp. Both epoch
/// nanos. Sum/Histogram carry both; Gauge ignores start.
pub struct ConvertTimes {
    pub start_unix_nano: u64,
    pub now_unix_nano: u64,
}

#[derive(Clone, Copy)]
enum NumberKind {
    Sum,
    Gauge,
}

/// Histogram pieces accumulated across samples for one (name, label-set):
/// prometheus_parse hands the buckets as a base `Histogram` sample and the
/// `_sum`/`_count` as separate `Untyped` samples.
struct HistAccum {
    name: String,
    attrs: Vec<KeyValue>,
    labels: Vec<StringKeyValue>,
    /// (less_than, cumulative_count) including the +Inf bucket.
    buckets: Vec<(f64, f64)>,
    sum: Option<f64>,
    count: Option<f64>,
}

/// Parse `prometheus_text` (the `/v1/metrics` render) and build one
/// `ExportMetricsServiceRequest` tagged with `resource`.
pub fn prometheus_to_otlp(
    prometheus_text: &str,
    resource: Resource,
    times: &ConvertTimes,
) -> anyhow::Result<ExportMetricsServiceRequest> {
    let lines = prometheus_text.lines().map(|l| io::Result::Ok(l.to_owned()));
    let scrape = Scrape::parse(lines).map_err(|e| anyhow::anyhow!("parse prometheus text: {e}"))?;

    // Per-instance labels mirrored onto EVERY data point: the platform filters/
    // groups by data-point labels, not by resource attributes, so ip/host must
    // ride on each point to distinguish instances. Pulled from the resource we
    // were handed (built from env.yaml + the running host).
    let ip = resource_attr(&resource, "ip").unwrap_or_default();
    let host = resource_attr(&resource, "host").unwrap_or_default();
    let instance: Vec<(&str, &str)> = vec![("ip", ip.as_str()), ("host", host.as_str())];

    // counter/gauge → NumberDataPoint, grouped by metric name (first-seen order).
    let mut num_order: Vec<String> = Vec::new();
    let mut num_acc: HashMap<String, (NumberKind, Vec<NumberDataPoint>)> = HashMap::new();

    // histogram buckets keyed by (base name, label signature) so distinct label
    // sets stay separate; grouped by name into one Metric at the end.
    let mut hist_order: Vec<(String, String)> = Vec::new();
    let mut hist_acc: HashMap<(String, String), HistAccum> = HashMap::new();

    // `_sum` / `_count` arrive as separate Untyped samples; stash and attach
    // after the first pass (the base Histogram sample may come before or after).
    let mut untyped: Vec<(String, String, f64)> = Vec::new();

    for sample in &scrape.samples {
        match &sample.value {
            Value::Counter(v) => {
                push_number(&mut num_order, &mut num_acc, sample, NumberKind::Sum, *v, times, &instance)
            }
            Value::Gauge(v) => {
                push_number(&mut num_order, &mut num_acc, sample, NumberKind::Gauge, *v, times, &instance)
            }
            Value::Histogram(buckets) => {
                let key = (sample.metric.clone(), label_key(&sample.labels));
                if !hist_acc.contains_key(&key) {
                    hist_order.push(key.clone());
                }
                let (attrs, labels) = dims(&sample.labels, &instance);
                let entry = hist_acc.entry(key).or_insert_with(|| HistAccum {
                    name: sample.metric.clone(),
                    attrs,
                    labels,
                    buckets: Vec::new(),
                    sum: None,
                    count: None,
                });
                entry.buckets = buckets.iter().map(|hc| (hc.less_than, hc.count)).collect();
            }
            Value::Untyped(v) => untyped.push((sample.metric.clone(), label_key(&sample.labels), *v)),
            Value::Summary(_) => {}
        }
    }

    // Attach _sum/_count to their histogram (matched by base name + label set).
    // Orphan Untyped (no matching histogram) is ignored — veda emits none.
    for (metric, lkey, v) in &untyped {
        if let Some(base) = metric.strip_suffix("_sum") {
            if let Some(e) = hist_acc.get_mut(&(base.to_string(), lkey.clone())) {
                e.sum = Some(*v);
            }
        } else if let Some(base) = metric.strip_suffix("_count") {
            if let Some(e) = hist_acc.get_mut(&(base.to_string(), lkey.clone())) {
                e.count = Some(*v);
            }
        }
    }

    let mut metrics: Vec<Metric> = Vec::new();

    // Number metrics (counter → Sum, gauge → Gauge).
    for name in &num_order {
        let (kind, data_points) = num_acc.remove(name).expect("name present in num_acc");
        let data = match kind {
            NumberKind::Sum => metric::Data::Sum(Sum {
                data_points,
                aggregation_temporality: AggregationTemporality::Cumulative as i32,
                is_monotonic: true,
            }),
            NumberKind::Gauge => metric::Data::Gauge(Gauge { data_points }),
        };
        metrics.push(Metric {
            name: name.clone(),
            description: String::new(),
            unit: String::new(),
            data: Some(data),
        });
    }

    // Histogram metrics: one Metric per base name, N data points (one per label set).
    let mut hist_name_order: Vec<String> = Vec::new();
    let mut hist_by_name: HashMap<String, Vec<HistogramDataPoint>> = HashMap::new();
    for key in &hist_order {
        let acc = hist_acc.remove(key).expect("hist key present");
        let name = acc.name.clone();
        let dp = build_histogram_dp(acc, times);
        if !hist_by_name.contains_key(&name) {
            hist_name_order.push(name.clone());
        }
        hist_by_name.entry(name).or_default().push(dp);
    }
    for name in &hist_name_order {
        let data_points = hist_by_name.remove(name).expect("name present in hist_by_name");
        metrics.push(Metric {
            name: name.clone(),
            description: String::new(),
            unit: String::new(),
            data: Some(metric::Data::Histogram(Histogram {
                data_points,
                aggregation_temporality: AggregationTemporality::Cumulative as i32,
            })),
        });
    }

    Ok(ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(resource),
            instrumentation_library_metrics: vec![InstrumentationLibraryMetrics {
                instrumentation_library: Some(InstrumentationLibrary {
                    name: INSTRUMENTATION_NAME.to_string(),
                    version: String::new(),
                }),
                metrics,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    })
}

fn push_number(
    order: &mut Vec<String>,
    acc: &mut HashMap<String, (NumberKind, Vec<NumberDataPoint>)>,
    sample: &Sample,
    kind: NumberKind,
    value: f64,
    times: &ConvertTimes,
    instance: &[(&str, &str)],
) {
    let dp = number_datapoint(&sample.labels, value, times, instance);
    acc.entry(sample.metric.clone())
        .or_insert_with(|| {
            order.push(sample.metric.clone());
            (kind, Vec::new())
        })
        .1
        .push(dp);
}

/// Stable signature of a label set (sorted `k=v` pairs) used to match a
/// histogram's `_sum`/`_count` to its buckets. Uses control chars as separators
/// that can't appear in label names/values.
fn label_key(labels: &Labels) -> String {
    let mut pairs: Vec<(&str, &str)> = labels.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}\u{1f}{v}"))
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

/// Pull a string-valued resource attribute (e.g. "ip"/"host") so it can be
/// mirrored onto every data point as a queryable label.
fn resource_attr(resource: &Resource, key: &str) -> Option<String> {
    resource.attributes.iter().find(|kv| kv.key == key).and_then(|kv| {
        match kv.value.as_ref()?.value.as_ref()? {
            any_value::Value::StringValue(s) => Some(s.clone()),
            _ => None,
        }
    })
}

/// Dual-write dimensions: same key-sorted pairs as both `attributes` (KeyValue)
/// and deprecated `labels` (StringKeyValue).
fn dims(labels: &Labels, instance: &[(&str, &str)]) -> (Vec<KeyValue>, Vec<StringKeyValue>) {
    let mut pairs: Vec<(String, String)> = labels
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    // Mirror per-instance labels (ip/host) onto the point; skip empties.
    for (k, v) in instance {
        if !v.is_empty() {
            pairs.push((k.to_string(), v.to_string()));
        }
    }
    pairs.sort();
    let attrs = pairs
        .iter()
        .map(|(k, v)| KeyValue {
            key: k.clone(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(v.clone())),
            }),
        })
        .collect();
    let labels = pairs
        .iter()
        .map(|(k, v)| StringKeyValue {
            key: k.clone(),
            value: v.clone(),
        })
        .collect();
    (attrs, labels)
}

fn number_datapoint(
    labels: &Labels,
    value: f64,
    times: &ConvertTimes,
    instance: &[(&str, &str)],
) -> NumberDataPoint {
    let (attributes, labels) = dims(labels, instance);
    NumberDataPoint {
        attributes,
        labels,
        start_time_unix_nano: times.start_unix_nano,
        time_unix_nano: times.now_unix_nano,
        value: Some(number_data_point::Value::AsDouble(value)),
        exemplars: Vec::new(),
    }
}

/// Build a HistogramDataPoint, diffing Prometheus cumulative buckets into OTLP
/// per-bucket counts. Prometheus `_bucket{le}` is cumulative (≤ le) and includes
/// a +Inf bucket; OTLP wants per-bucket counts with `explicit_bounds` excluding
/// +Inf and the +Inf bucket as the trailing `bucket_counts` entry, satisfying
/// `bucket_counts.len == explicit_bounds.len + 1`.
fn build_histogram_dp(acc: HistAccum, times: &ConvertTimes) -> HistogramDataPoint {
    let mut finite: Vec<(f64, f64)> = acc
        .buckets
        .iter()
        .copied()
        .filter(|(le, _)| le.is_finite())
        .collect();
    finite.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let inf_cumulative = acc
        .buckets
        .iter()
        .find(|(le, _)| le.is_infinite())
        .map(|(_, c)| *c);

    let explicit_bounds: Vec<f64> = finite.iter().map(|(le, _)| *le).collect();
    let mut bucket_counts: Vec<u64> = Vec::with_capacity(finite.len() + 1);
    let mut prev = 0.0_f64;
    for (_, cumulative) in &finite {
        bucket_counts.push((cumulative - prev).max(0.0).round() as u64);
        prev = *cumulative;
    }
    // total = _count (authoritative), else the +Inf cumulative, else last finite.
    let total = acc.count.or(inf_cumulative).unwrap_or(prev);
    bucket_counts.push((total - prev).max(0.0).round() as u64);

    HistogramDataPoint {
        attributes: acc.attrs,
        labels: acc.labels,
        start_time_unix_nano: times.start_unix_nano,
        time_unix_nano: times.now_unix_nano,
        count: total.round() as u64,
        sum: acc.sum.unwrap_or(0.0),
        bucket_counts,
        explicit_bounds,
        exemplars: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_resource() -> Resource {
        Resource {
            attributes: Vec::new(),
            dropped_attributes_count: 0,
        }
    }

    #[test]
    fn counter_maps_to_monotonic_cumulative_sum_with_dual_labels() {
        let text = "\
# TYPE veda_http_requests_total counter
veda_http_requests_total{route=\"/v1/foo\",method=\"GET\",status=\"200\"} 5
";
        let times = ConvertTimes {
            start_unix_nano: 100,
            now_unix_nano: 200,
        };
        let req = prometheus_to_otlp(text, empty_resource(), &times).unwrap();

        let ilm = &req.resource_metrics[0].instrumentation_library_metrics[0];
        assert_eq!(ilm.instrumentation_library.as_ref().unwrap().name, "monitor");
        assert_eq!(ilm.metrics.len(), 1);
        let m = &ilm.metrics[0];
        assert_eq!(m.name, "veda_http_requests_total");

        let sum = match m.data.as_ref().unwrap() {
            metric::Data::Sum(s) => s,
            other => panic!("expected Sum, got {other:?}"),
        };
        assert!(sum.is_monotonic);
        assert_eq!(
            sum.aggregation_temporality,
            AggregationTemporality::Cumulative as i32
        );
        assert_eq!(sum.data_points.len(), 1);
        let dp = &sum.data_points[0];
        assert_eq!(dp.value, Some(number_data_point::Value::AsDouble(5.0)));
        assert_eq!(dp.start_time_unix_nano, 100);
        assert_eq!(dp.time_unix_nano, 200);
        assert_eq!(dp.attributes.len(), 3);
        assert_eq!(dp.labels.len(), 3);
        let label_keys: Vec<&str> = dp.labels.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(label_keys, vec!["method", "route", "status"]);
        let method = dp.labels.iter().find(|l| l.key == "method").unwrap();
        assert_eq!(method.value, "GET");
        let method_attr = dp.attributes.iter().find(|a| a.key == "method").unwrap();
        assert_eq!(
            method_attr.value.as_ref().unwrap().value,
            Some(any_value::Value::StringValue("GET".to_string()))
        );
    }

    #[test]
    fn instance_ip_host_mirrored_onto_datapoint_labels() {
        let text = "\
# TYPE veda_http_requests_total counter
veda_http_requests_total{route=\"/v1/foo\"} 1
";
        let kv = |k: &str, v: &str| KeyValue {
            key: k.to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(v.to_string())),
            }),
        };
        let res = Resource {
            attributes: vec![
                kv("appname", "dbpaas-ai-service"),
                kv("ip", "10.79.51.161"),
                kv("host", "box-161"),
            ],
            dropped_attributes_count: 0,
        };
        let times = ConvertTimes {
            start_unix_nano: 0,
            now_unix_nano: 1,
        };
        let req = prometheus_to_otlp(text, res, &times).unwrap();
        let dp = match req.resource_metrics[0].instrumentation_library_metrics[0].metrics[0]
            .data
            .as_ref()
            .unwrap()
        {
            metric::Data::Sum(s) => &s.data_points[0],
            other => panic!("expected Sum, got {other:?}"),
        };
        // ip + host mirrored onto the point's labels (and attributes) alongside
        // the metric's own dims.
        let label_keys: Vec<&str> = dp.labels.iter().map(|l| l.key.as_str()).collect();
        assert!(label_keys.contains(&"ip"), "labels missing ip: {label_keys:?}");
        assert!(label_keys.contains(&"host"), "labels missing host: {label_keys:?}");
        assert!(label_keys.contains(&"route"));
        assert_eq!(
            dp.labels.iter().find(|l| l.key == "ip").unwrap().value,
            "10.79.51.161"
        );
        assert!(dp.attributes.iter().any(|a| a.key == "ip"));
        assert!(dp.attributes.iter().any(|a| a.key == "host"));
    }

    #[test]
    fn gauge_maps_to_gauge() {
        let text = "\
# TYPE veda_mysql_pool_idle gauge
veda_mysql_pool_idle 3
";
        let times = ConvertTimes {
            start_unix_nano: 0,
            now_unix_nano: 50,
        };
        let req = prometheus_to_otlp(text, empty_resource(), &times).unwrap();
        let m = &req.resource_metrics[0].instrumentation_library_metrics[0].metrics[0];
        let gauge = match m.data.as_ref().unwrap() {
            metric::Data::Gauge(g) => g,
            other => panic!("expected Gauge, got {other:?}"),
        };
        assert_eq!(gauge.data_points.len(), 1);
        assert_eq!(
            gauge.data_points[0].value,
            Some(number_data_point::Value::AsDouble(3.0))
        );
        assert_eq!(gauge.data_points[0].time_unix_nano, 50);
    }

    #[test]
    fn histogram_diffs_cumulative_buckets_and_attaches_sum_count() {
        let text = "\
# TYPE veda_http_request_duration_seconds histogram
veda_http_request_duration_seconds_bucket{route=\"/x\",le=\"0.1\"} 2
veda_http_request_duration_seconds_bucket{route=\"/x\",le=\"0.5\"} 5
veda_http_request_duration_seconds_bucket{route=\"/x\",le=\"+Inf\"} 8
veda_http_request_duration_seconds_sum{route=\"/x\"} 1.23
veda_http_request_duration_seconds_count{route=\"/x\"} 8
";
        let times = ConvertTimes {
            start_unix_nano: 10,
            now_unix_nano: 20,
        };
        let req = prometheus_to_otlp(text, empty_resource(), &times).unwrap();
        let ilm = &req.resource_metrics[0].instrumentation_library_metrics[0];
        assert_eq!(ilm.metrics.len(), 1);
        let m = &ilm.metrics[0];
        assert_eq!(m.name, "veda_http_request_duration_seconds");

        let h = match m.data.as_ref().unwrap() {
            metric::Data::Histogram(h) => h,
            other => panic!("expected Histogram, got {other:?}"),
        };
        assert_eq!(
            h.aggregation_temporality,
            AggregationTemporality::Cumulative as i32
        );
        assert_eq!(h.data_points.len(), 1);
        let dp = &h.data_points[0];
        // +Inf excluded from bounds; cumulative [2,5,8] → per-bucket [2,3,3].
        assert_eq!(dp.explicit_bounds, vec![0.1, 0.5]);
        assert_eq!(dp.bucket_counts, vec![2, 3, 3]);
        assert_eq!(dp.bucket_counts.len(), dp.explicit_bounds.len() + 1);
        // The whole point of diffing: sum(buckets) == count (not > count).
        assert_eq!(dp.bucket_counts.iter().sum::<u64>(), dp.count);
        assert_eq!(dp.count, 8);
        assert_eq!(dp.sum, 1.23);
        assert_eq!(dp.start_time_unix_nano, 10);
        assert_eq!(dp.time_unix_nano, 20);
        // dims dual-written.
        assert_eq!(dp.attributes.len(), 1);
        assert_eq!(dp.labels.len(), 1);
        assert_eq!(dp.labels[0].key, "route");
        assert_eq!(dp.labels[0].value, "/x");
    }

    #[test]
    fn histogram_keeps_label_sets_as_separate_datapoints() {
        let text = "\
# TYPE veda_http_request_duration_seconds histogram
veda_http_request_duration_seconds_bucket{route=\"/a\",le=\"0.1\"} 1
veda_http_request_duration_seconds_bucket{route=\"/a\",le=\"+Inf\"} 1
veda_http_request_duration_seconds_sum{route=\"/a\"} 0.05
veda_http_request_duration_seconds_count{route=\"/a\"} 1
veda_http_request_duration_seconds_bucket{route=\"/b\",le=\"0.1\"} 0
veda_http_request_duration_seconds_bucket{route=\"/b\",le=\"+Inf\"} 4
veda_http_request_duration_seconds_sum{route=\"/b\"} 9.0
veda_http_request_duration_seconds_count{route=\"/b\"} 4
";
        let times = ConvertTimes {
            start_unix_nano: 0,
            now_unix_nano: 1,
        };
        let req = prometheus_to_otlp(text, empty_resource(), &times).unwrap();
        let ilm = &req.resource_metrics[0].instrumentation_library_metrics[0];
        // One Metric, two data points (route=/a and route=/b).
        assert_eq!(ilm.metrics.len(), 1);
        let h = match ilm.metrics[0].data.as_ref().unwrap() {
            metric::Data::Histogram(h) => h,
            other => panic!("expected Histogram, got {other:?}"),
        };
        assert_eq!(h.data_points.len(), 2);
        for dp in &h.data_points {
            assert_eq!(dp.bucket_counts.iter().sum::<u64>(), dp.count);
        }
    }
}
