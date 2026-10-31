//! How much the panel sent to each phone and got from it: one meter per sign-in (a paired phone and this
//! computer have one each), split by way (the tunnel or the home network) and kind (live view, files, the rest).
//! Only what StrCu itself sends and receives is counted; encryption and headers on the way add a little.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::response::Response;
use http_body::{Body as HttpBody, Frame, SizeHint};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Way {
    Tunnel,
    Home,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    View,
    Files,
    Other,
}

const WAYS: [Way; 2] = [Way::Tunnel, Way::Home];
const KINDS: [Kind; 3] = [Kind::View, Kind::Files, Kind::Other];

/// What a request is for, by its path.
pub fn kind(path: &str) -> Kind {
    match path {
        "/api/stream" | "/api/shot" | "/api/macros/shot" => Kind::View,
        p if p == "/api/files" || p.starts_with("/api/files/") => Kind::Files,
        _ => Kind::Other,
    }
}

/// Bytes to the phone and from it, for each way and kind, since `since`.
pub struct Meter {
    since: Instant,
    n: [AtomicU64; 12],
}

impl Default for Meter {
    fn default() -> Self {
        Meter { since: Instant::now(), n: Default::default() }
    }
}

impl Meter {
    fn slot(way: Way, kind: Kind, up: bool) -> usize {
        (way as usize * KINDS.len() + kind as usize) * 2 + up as usize
    }

    pub fn add(&self, way: Way, kind: Kind, up: bool, bytes: u64) {
        if bytes > 0 {
            self.n[Self::slot(way, kind, up)].fetch_add(bytes, Ordering::Relaxed);
            if way == Way::Tunnel {
                TUNNEL_AT.store(EPOCH.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
        }
    }

    /// [to the phone, from it] over the given ways and kinds.
    fn sum(&self, ways: &[Way], kinds: &[Kind]) -> [u64; 2] {
        [false, true].map(|up| {
            ways.iter().flat_map(|&w| kinds.iter().map(move |&k| Self::slot(w, k, up))).map(|i| self.n[i].load(Ordering::Relaxed)).sum()
        })
    }

    /// For the panel: [to the phone, from it] by way and by kind, and for how many seconds it has been counting.
    pub fn report(&self) -> Value {
        json!({
            "secs": self.since.elapsed().as_secs(),
            "tunnel": self.sum(&[Way::Tunnel], &KINDS),
            "home": self.sum(&[Way::Home], &KINDS),
            "view": self.sum(&WAYS, &[Kind::View]),
            "files": self.sum(&WAYS, &[Kind::Files]),
            "other": self.sum(&WAYS, &[Kind::Other]),
        })
    }
}

/// When something last went through the tunnel, either way, in milliseconds from `EPOCH`.
static TUNNEL_AT: AtomicU64 = AtomicU64::new(0);
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// How long nothing has gone through the tunnel (cloudflared is changed over only when nobody is using it).
pub fn tunnel_quiet() -> Duration {
    EPOCH.elapsed().saturating_sub(Duration::from_millis(TUNNEL_AT.load(Ordering::Relaxed)))
}

/// The meters, by whose they are ("s:" a sign-in, "d:" a paired phone, "local" this computer).
#[derive(Default)]
pub struct Usage {
    meters: Mutex<HashMap<String, Arc<Meter>>>,
}

impl Usage {
    /// The meter for `key`; a new one starts at zero. When one is added, those whose sign-in is over (`alive`
    /// says no) go.
    pub fn meter(&self, key: &str, alive: impl Fn(&str) -> bool) -> Arc<Meter> {
        let mut m = self.meters.lock().unwrap();
        if let Some(x) = m.get(key) {
            return x.clone();
        }
        m.retain(|k, _| alive(k));
        m.entry(key.to_string()).or_default().clone()
    }
}

/// One request's share of a meter: which way it came and what it is for.
#[derive(Clone)]
pub struct Tap {
    pub meter: Arc<Meter>,
    way: Way,
    kind: Kind,
}

impl Tap {
    pub fn new(meter: Arc<Meter>, way: Way, kind: Kind) -> Self {
        Tap { meter, way, kind }
    }

    /// Sent to the phone
    pub fn down(&self, bytes: usize) {
        self.meter.add(self.way, self.kind, false, bytes as u64);
    }

    /// Received from the phone
    pub fn up(&self, bytes: usize) {
        self.meter.add(self.way, self.kind, true, bytes as u64);
    }

    /// The response, its body counted piece by piece as it goes out (a download cut short counts what went).
    pub fn count(&self, r: Response) -> Response {
        let tap = self.clone();
        r.map(|inner| Body::new(Counted { inner, tap }))
    }
}

struct Counted {
    inner: Body,
    tap: Tap,
}

impl HttpBody for Counted {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let p = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(f))) = &p
            && let Some(d) = f.data_ref()
        {
            self.tap.down(d.len());
        }
        p
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_sorted_by_path() {
        assert_eq!(kind("/api/stream"), Kind::View);
        assert_eq!(kind("/api/shot"), Kind::View);
        assert_eq!(kind("/api/files"), Kind::Files);
        assert_eq!(kind("/api/files/upload"), Kind::Files);
        assert_eq!(kind("/api/filesx"), Kind::Other);
        assert_eq!(kind("/api/click"), Kind::Other);
        assert_eq!(kind("/"), Kind::Other);
    }

    #[test]
    fn meters_add_up_by_way_and_kind() {
        let m = Meter::default();
        m.add(Way::Tunnel, Kind::View, false, 1000);
        m.add(Way::Tunnel, Kind::Other, true, 30);
        m.add(Way::Home, Kind::Files, false, 500);
        m.add(Way::Home, Kind::Files, true, 7);
        let r = m.report();
        assert_eq!(r["tunnel"], json!([1000, 30]));
        assert_eq!(r["home"], json!([500, 7]));
        assert_eq!(r["view"], json!([1000, 0]));
        assert_eq!(r["files"], json!([500, 7]));
        assert_eq!(r["other"], json!([0, 30]));
    }

    #[test]
    fn a_meter_lasts_as_long_as_its_sign_in() {
        let u = Usage::default();
        let a = u.meter("s:a", |_| true);
        a.add(Way::Home, Kind::Other, false, 5);
        assert!(Arc::ptr_eq(&a, &u.meter("s:a", |_| true)));
        // A new sign-in clears out the ones that ended
        u.meter("s:b", |k| k != "s:a");
        assert_eq!(u.meter("s:a", |_| true).report()["home"], json!([0, 0]));
    }

    #[tokio::test]
    async fn bodies_are_counted_as_they_go_out() {
        let tap = Tap::new(Arc::default(), Way::Tunnel, Kind::Files);
        let r = tap.count(Response::new(Body::from("hello world")));
        assert_eq!(r.into_body().size_hint().exact(), Some(11));
        let r = tap.count(Response::new(Body::from("hello world")));
        let got = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        assert_eq!(got.len(), 11);
        assert_eq!(tap.meter.report()["files"], json!([11, 0]));
    }
}
