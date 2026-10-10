//! Delivers the lab outbox to the Lab's `POST /internal/events` (hosted only):
//! batches of up to 500 events, signed `X-Lab-Signature: sha256=<hex
//! HMAC-SHA256(LAB_INSTANCE_SECRET, "<X-Lab-Timestamp>.<raw body>")>` and
//! identified by `X-Lab-Instance-Id`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{error, info, warn};
use uuid::Uuid;

use crate::job_store::StoreError;
use crate::lab_events::LabOutbox;

pub const BATCH: i64 = 500;
/// Spec §3.4: an event the Lab never acknowledged for this long is
/// permanently rejected; drop it (error log) instead of retrying forever.
const MAX_AGE: chrono::TimeDelta = chrono::TimeDelta::hours(24);
const TICK: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const PURGE_EVERY: Duration = Duration::from_secs(3600);
const PURGE_OLDER_THAN_SECS: i64 = 7 * 24 * 3600;
const STALE_AFTER: chrono::TimeDelta = chrono::TimeDelta::minutes(10);
const STALE_WARN_EVERY: Duration = Duration::from_secs(60);

pub struct LabSink {
    outbox: Arc<dyn LabOutbox>,
    client: reqwest::Client,
    url: String,
    instance_id: String,
    secret: String,
}

#[derive(serde::Deserialize)]
struct Ack {
    #[serde(default)]
    accepted: Vec<Uuid>,
}

/// The client the sink should be built with: like `LabClient`'s, it never
/// follows a redirect (a 3xx is a misconfigured `LAB_URL`, and following it
/// would send the signed batch elsewhere), so a 3xx counts as an
/// undelivered batch and is retried with backoff.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client")
}

impl LabSink {
    pub fn new(
        outbox: Arc<dyn LabOutbox>,
        client: reqwest::Client,
        url: String,
        instance_id: String,
        secret: String,
    ) -> Self {
        Self {
            outbox,
            client,
            url,
            instance_id,
            secret,
        }
    }

    /// Send one batch of due events. Accepted events are marked delivered,
    /// the rest are rescheduled with backoff; events older than 24 h are
    /// dropped instead of sent. Returns how many were accepted.
    pub async fn deliver_once(&self) -> Result<usize, StoreError> {
        let now = chrono::Utc::now();
        let (expired, events): (Vec<_>, Vec<_>) = self
            .outbox
            .due(BATCH)
            .await?
            .into_iter()
            .partition(|e| now - e.occurred_at > MAX_AGE);
        if !expired.is_empty() {
            for e in &expired {
                error!(
                    event_id = %e.id,
                    account_id = %e.account_id,
                    event_type = %e.kind,
                    operation = e.data.get("operation").and_then(|v| v.as_str()),
                    occurred_at = %e.occurred_at,
                    "Lab event never acknowledged for 24 h: dropped (spec 3.4)"
                );
            }
            let ids: Vec<Uuid> = expired.iter().map(|e| e.id).collect();
            let dropped = self.outbox.abandon(&ids).await?;
            scrapix_core::metrics::lab_events_delivered_total()
                .with_label_values(&["dropped"])
                .inc_by(dropped as f64);
        }
        if events.is_empty() {
            return Ok(0);
        }
        let ids: Vec<Uuid> = events.iter().map(|e| e.id).collect();
        let body = serde_json::to_vec(&serde_json::json!({ "events": events }))
            .map_err(|e| StoreError::Other(e.to_string()))?;
        // Signed with the time of sending (the Lab rejects a timestamp more
        // than 300 s off), never a cached one.
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let mut signed = format!("{timestamp}.").into_bytes();
        signed.extend_from_slice(&body);
        let signature = crate::webhooks::sign_sha256(self.secret.as_bytes(), &signed);
        // Not-accepted events count as `rejected` when the Lab answered but
        // did not take them, `failed` when it could not be reached or read.
        let (accepted, outcome): (Vec<Uuid>, &str) = match self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("X-Lab-Instance-Id", &self.instance_id)
            .header("X-Lab-Timestamp", &timestamp)
            .header("X-Lab-Signature", signature)
            .timeout(REQUEST_TIMEOUT)
            .body(body)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => match r.json::<Ack>().await {
                Ok(ack) => (
                    ack.accepted
                        .into_iter()
                        .filter(|id| ids.contains(id))
                        .collect(),
                    "rejected",
                ),
                Err(e) => {
                    warn!(error = %e, "Lab events: unreadable acknowledgement");
                    (Vec::new(), "failed")
                }
            },
            Ok(r) => {
                warn!(status = %r.status(), "Lab events: delivery rejected");
                (Vec::new(), "rejected")
            }
            Err(e) => {
                warn!(error = %e, "Lab events: delivery failed");
                (Vec::new(), "failed")
            }
        };
        let rest: Vec<Uuid> = ids
            .iter()
            .copied()
            .filter(|id| !accepted.contains(id))
            .collect();
        self.outbox.mark_delivered(&accepted).await?;
        self.outbox.reschedule(&rest).await?;
        let m = scrapix_core::metrics::lab_events_delivered_total();
        m.with_label_values(&["accepted"])
            .inc_by(accepted.len() as f64);
        m.with_label_values(&[outcome]).inc_by(rest.len() as f64);
        Ok(accepted.len())
    }

    /// Deliver until `shutdown` fires (one last attempt on the way out; what
    /// remains stays in the outbox for the next process).
    pub fn spawn(
        self: Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut last_purge = Instant::now();
            let mut last_stale_warn: Option<Instant> = None;
            loop {
                let full = match self.deliver_once().await {
                    Ok(n) => n as i64 == BATCH,
                    Err(e) => {
                        warn!(error = %e, "Lab events: outbox error");
                        false
                    }
                };
                if let Ok((pending, oldest)) = self.outbox.pending_stats().await {
                    scrapix_core::metrics::lab_events_pending().set(pending as f64);
                    if let Some(oldest) = oldest {
                        if chrono::Utc::now() - oldest > STALE_AFTER
                            && last_stale_warn.is_none_or(|t| t.elapsed() >= STALE_WARN_EVERY)
                        {
                            warn!(
                                pending,
                                oldest = %oldest,
                                "Lab events backlog older than 10 minutes — is the Lab reachable?"
                            );
                            last_stale_warn = Some(Instant::now());
                        }
                    }
                }
                if last_purge.elapsed() >= PURGE_EVERY {
                    if let Ok(n) = self.outbox.purge_delivered(PURGE_OLDER_THAN_SECS).await {
                        if n > 0 {
                            info!(purged = n, "Purged delivered lab events");
                        }
                    }
                    last_purge = Instant::now();
                }
                if *shutdown.borrow() {
                    let _ = self.deliver_once().await;
                    return;
                }
                if full {
                    continue;
                }
                tokio::select! {
                    _ = tokio::time::sleep(TICK) => {}
                    _ = shutdown.changed() => {
                        let _ = self.deliver_once().await;
                        return;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab_events::{LabEvent, MemoryOutbox};
    use serde_json::json;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const INSTANCE_ID: &str = "0f0f0f0f-0f0f-4f0f-8f0f-0f0f0f0f0f0f";
    const SECRET: &str = "abababababababababababababababababababababababababababababababab";

    fn ev() -> LabEvent {
        LabEvent::usage(
            "7f1c2a8e-0000-4000-8000-000000000001",
            None,
            "map",
            json!({}),
            "m".into(),
            None,
        )
    }

    struct AcceptAll;
    impl Respond for AcceptAll {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let ids: Vec<_> = body["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["id"].clone())
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "accepted": ids }))
        }
    }

    fn sink(outbox: Arc<MemoryOutbox>, url: String) -> LabSink {
        LabSink::new(
            outbox,
            http_client(),
            url,
            INSTANCE_ID.into(),
            SECRET.into(),
        )
    }

    #[tokio::test]
    async fn signature_covers_timestamp_dot_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/events"))
            .and(header_exists("X-Lab-Signature"))
            .and(header_exists("X-Lab-Timestamp"))
            .and(header("X-Lab-Instance-Id", INSTANCE_ID))
            .respond_with(AcceptAll)
            .expect(1)
            .mount(&server)
            .await;
        let outbox = Arc::new(MemoryOutbox::default());
        outbox.enqueue(&[ev(), ev()]).await.unwrap();
        let s = sink(outbox.clone(), format!("{}/internal/events", server.uri()));
        let before = chrono::Utc::now().timestamp();
        assert_eq!(s.deliver_once().await.unwrap(), 2);
        assert!(outbox.due(10).await.unwrap().is_empty());
        let req = &server.received_requests().await.unwrap()[0];
        let ts = req
            .headers
            .get("X-Lab-Timestamp")
            .unwrap()
            .to_str()
            .unwrap();
        let ts_num: i64 = ts.parse().unwrap();
        assert!(
            (before..=chrono::Utc::now().timestamp()).contains(&ts_num),
            "signed with the current time"
        );
        let sig = req
            .headers
            .get("X-Lab-Signature")
            .unwrap()
            .to_str()
            .unwrap();
        let mut signed = format!("{ts}.").into_bytes();
        signed.extend_from_slice(&req.body);
        assert_eq!(
            sig,
            crate::webhooks::sign_sha256(SECRET.as_bytes(), &signed)
        );
        assert!(
            req.headers.get("X-Scrapix-Signature").is_none(),
            "v1 header gone"
        );
    }

    #[tokio::test]
    async fn batches_hold_up_to_500_events() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(AcceptAll)
            .expect(2)
            .mount(&server)
            .await;
        let outbox = Arc::new(MemoryOutbox::default());
        let events: Vec<_> = (0..501).map(|_| ev()).collect();
        outbox.enqueue(&events).await.unwrap();
        let s = sink(outbox.clone(), server.uri());
        assert_eq!(s.deliver_once().await.unwrap(), 500);
        assert_eq!(s.deliver_once().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn events_older_than_24h_are_dropped_with_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .expect(0)
            .mount(&server)
            .await;
        let outbox = Arc::new(MemoryOutbox::default());
        let mut stale = ev();
        stale.occurred_at = chrono::Utc::now() - chrono::TimeDelta::hours(25);
        outbox.enqueue(&[stale.clone()]).await.unwrap();
        let s = sink(outbox.clone(), server.uri());
        let dropped =
            scrapix_core::metrics::lab_events_delivered_total().with_label_values(&["dropped"]);
        let before = dropped.get();
        assert_eq!(s.deliver_once().await.unwrap(), 0);
        assert_eq!(
            outbox.pending_stats().await.unwrap().0,
            0,
            "abandoned, not retried"
        );
        assert!(dropped.get() > before);
    }

    #[tokio::test]
    async fn partial_acceptance_reschedules_the_rest() {
        let server = MockServer::start().await;
        let (a, b) = (ev(), ev());
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "accepted": [a.id] })))
            .mount(&server)
            .await;
        let outbox = Arc::new(MemoryOutbox::default());
        outbox.enqueue(&[a.clone(), b.clone()]).await.unwrap();
        assert_eq!(
            sink(outbox.clone(), server.uri())
                .deliver_once()
                .await
                .unwrap(),
            1
        );
        assert_eq!(outbox.pending_stats().await.unwrap().0, 1);
        assert!(outbox.due(10).await.unwrap().is_empty(), "b is backed off");
    }

    #[tokio::test]
    async fn server_error_and_unreachable_back_off_everything() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let outbox = Arc::new(MemoryOutbox::default());
        outbox.enqueue(&[ev()]).await.unwrap();
        assert_eq!(
            sink(outbox.clone(), server.uri())
                .deliver_once()
                .await
                .unwrap(),
            0
        );
        assert!(outbox.due(10).await.unwrap().is_empty());
        let outbox2 = Arc::new(MemoryOutbox::default());
        outbox2.enqueue(&[ev()]).await.unwrap();
        assert_eq!(
            sink(outbox2.clone(), "http://127.0.0.1:1".into())
                .deliver_once()
                .await
                .unwrap(),
            0
        );
        assert_eq!(outbox2.pending_stats().await.unwrap().0, 1);
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/events"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/elsewhere", server.uri())),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/elsewhere"))
            .respond_with(AcceptAll)
            .expect(0)
            .mount(&server)
            .await;
        let outbox = Arc::new(MemoryOutbox::default());
        outbox.enqueue(&[ev()]).await.unwrap();
        let s = sink(outbox.clone(), format!("{}/internal/events", server.uri()));
        assert_eq!(s.deliver_once().await.unwrap(), 0);
        assert_eq!(
            outbox.pending_stats().await.unwrap().0,
            1,
            "undelivered, retried later"
        );
    }

    #[tokio::test]
    async fn drains_backlog_in_consecutive_batches() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(AcceptAll)
            .expect(3)
            .mount(&server)
            .await;
        let outbox = Arc::new(MemoryOutbox::default());
        let events: Vec<_> = (0..1100).map(|_| ev()).collect();
        outbox.enqueue(&events).await.unwrap();
        let s = Arc::new(sink(outbox.clone(), server.uri()));
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let h = s.clone().spawn(rx);
        tokio::time::timeout(std::time::Duration::from_millis(900), async {
            while outbox.pending_stats().await.unwrap().0 > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a 1100-event backlog drains without 1 s pauses between full batches");
        h.abort();
        let sent: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .flat_map(|r| {
                serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["id"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            sent,
            events.iter().map(|e| e.id.to_string()).collect::<Vec<_>>(),
            "created_at order"
        );
    }

    #[tokio::test]
    async fn empty_outbox_sends_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(AcceptAll)
            .expect(0)
            .mount(&server)
            .await;
        assert_eq!(
            sink(Arc::new(MemoryOutbox::default()), server.uri())
                .deliver_once()
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn shutdown_makes_a_final_attempt_and_exits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(AcceptAll)
            .mount(&server)
            .await;
        let outbox = Arc::new(MemoryOutbox::default());
        let s = Arc::new(sink(outbox.clone(), server.uri()));
        let (tx, rx) = tokio::sync::watch::channel(false);
        let h = s.spawn(rx);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        outbox.enqueue(&[ev()]).await.unwrap();
        tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), h)
            .await
            .expect("sink exits on shutdown")
            .unwrap();
        assert_eq!(outbox.pending_stats().await.unwrap().0, 0);
    }
}
