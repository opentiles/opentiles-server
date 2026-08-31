//! Where the caches live: a local directory or an S3 bucket.
//!
//! Both cache tiers — the builder's input cache
//! (`{texture,heightmap}/z/x/y.png` plus `.404` markers) and the server's
//! output cache (`glb/{fingerprint}/z/x/y.glb`) — address entries by
//! `/`-separated keys and go through one [`Store`]. [`LocalStore`] maps keys
//! to files under a root directory (the layout raytiles / bevytiles share);
//! [`S3Store`] maps them to objects under an optional key prefix, so one
//! bucket can back any number of stateless server replicas.
//!
//! [`open`] picks the backend from a spec string: `s3://bucket[/prefix]` or
//! a directory path.
//!
//! The trait is blocking on purpose: the builder is synchronous (it fans out
//! with scoped threads and runs under `spawn_blocking` in the server), so a
//! blocking store slots in without an async boundary at every call site.

use crate::{Error, Result};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A flat key → bytes store with atomic publication. See the module docs.
pub trait Store: Send + Sync + fmt::Debug {
    /// The bytes at `key`, or `None` when there is no such entry.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Whether `key` exists (an empty entry counts).
    fn exists(&self, key: &str) -> Result<bool>;
    /// Publish `bytes` at `key` atomically: a concurrent reader sees the
    /// previous entry or the new one, never a partial write.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;
    /// Remove `key`; a missing entry is not an error.
    fn delete(&self, key: &str) -> Result<()>;
    /// Every key under `prefix` (recursively), sorted.
    fn list(&self, prefix: &str) -> Result<Vec<String>>;
    /// Human-readable location for logs: `.cache`, `s3://bucket/prefix`.
    fn location(&self) -> String;
}

/// Open the store a spec names: `s3://bucket[/prefix]` (feature `s3`) or a
/// local directory, created lazily on first write.
pub fn open(spec: &str) -> Result<Arc<dyn Store>> {
    if spec.starts_with("s3://") {
        #[cfg(feature = "s3")]
        {
            return Ok(Arc::new(S3Store::from_url(spec)?));
        }
        #[cfg(not(feature = "s3"))]
        {
            return Err(Error::Io {
                path: spec.into(),
                source: std::io::Error::other("open-tiles was built without the `s3` feature"),
            });
        }
    }
    Ok(Arc::new(LocalStore::new(spec)))
}

// -- local directory -----------------------------------------------------------

/// Keys as files under a root directory: `texture/5/3/4.png` →
/// `{root}/texture/5/3/4.png`.
#[derive(Clone, Debug)]
pub struct LocalStore {
    /// Directory every key lives under.
    root: PathBuf,
}

impl LocalStore {
    /// A store rooted at `root` (need not exist yet).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file a key maps to.
    pub fn path(&self, key: &str) -> PathBuf {
        let mut p = self.root.clone();
        p.extend(key.split('/').filter(|s| !s.is_empty()));
        p
    }

    /// Depth-first, sorted recursion collecting every file under `dir`.
    fn walk(&self, dir: &Path, out: &mut Vec<String>) -> Result<()> {
        for entry in read_dir_sorted(dir)? {
            if entry.is_dir() {
                self.walk(&entry, out)?;
            } else if let Some(key) = self.key_of(&entry) {
                out.push(key);
            }
        }
        Ok(())
    }

    /// Path → key: the `/`-joined components relative to the root. `None`
    /// for non-UTF-8 names, which this store never creates itself.
    fn key_of(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let parts: Option<Vec<&str>> = rel.iter().map(|c| c.to_str()).collect();
        Some(parts?.join("/"))
    }
}

impl Store for LocalStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path(key);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(&path, e)),
        }
    }

    fn exists(&self, key: &str) -> Result<bool> {
        let path = self.path(key);
        match std::fs::metadata(&path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err(&path, e)),
        }
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        write_atomic(&self.path(key), bytes)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let path = self.path(key);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(&path, e)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let start = self.path(prefix);
        let mut keys = Vec::new();
        match std::fs::metadata(&start) {
            Ok(m) if m.is_dir() => self.walk(&start, &mut keys)?,
            Ok(_) => keys.extend(self.key_of(&start)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&start, e)),
        }
        keys.sort();
        Ok(keys)
    }

    fn location(&self) -> String {
        self.root.display().to_string()
    }
}

/// Directory entries, sorted so listings are deterministic; a missing
/// directory is just an empty listing.
fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>> {
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            let mut v: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            v.sort();
            Ok(v)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(io_err(dir, e)),
    }
}

/// Shorthand for [`Error::Io`] with the path rendered into the message.
pub(crate) fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Write `bytes` to `path` atomically: unique temp file in the same
/// directory, then rename. Concurrent writers of the same path race on the
/// rename only, which is benign (identical bytes). The temp name includes
/// the process id: a CLI may run next to an engine sharing the same cache,
/// and a counter alone is only unique within one process.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, bytes).map_err(|e| io_err(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io_err(path, e)
    })
}

// -- S3 ------------------------------------------------------------------------

#[cfg(feature = "s3")]
pub use s3::S3Store;

/// The S3 backend (feature `s3`): the AWS SDK wrapped behind the blocking
/// [`Store`] trait via a store-owned tokio runtime.
#[cfg(feature = "s3")]
mod s3 {
    use super::*;
    use aws_sdk_s3::config::http::HttpResponse;
    use aws_sdk_s3::config::SharedHttpClient;
    use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
    use aws_sdk_s3::primitives::ByteStream;
    use aws_sdk_s3::Client;
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::runtime::{Handle, Runtime};

    /// Keys as objects in one bucket under an optional prefix:
    /// `texture/5/3/4.png` → `s3://bucket/{prefix/}texture/5/3/4.png`.
    ///
    /// Region, credentials and a custom endpoint come from the standard AWS
    /// environment (`AWS_REGION`, `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`,
    /// `AWS_PROFILE`, instance / task / pod roles, `AWS_ENDPOINT_URL[_S3]`).
    /// When a custom endpoint is set, path-style addressing is used, which is
    /// what MinIO, LocalStack and most other S3-compatible stores expect.
    ///
    /// The store owns a small tokio runtime for the SDK; every operation
    /// blocks the calling thread until the request completes, so it can be
    /// called from plain threads and from `spawn_blocking` alike.
    pub struct S3Store {
        /// `Some` until dropped; see the `Drop` impl.
        rt: Option<Runtime>,
        /// SDK client — cheap to clone, shares one connection pool.
        client: Client,
        /// Bucket every key maps into.
        bucket: String,
        /// `""` or `"some/prefix/"`.
        prefix: String,
    }

    impl fmt::Debug for S3Store {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("S3Store")
                .field("bucket", &self.bucket)
                .field("prefix", &self.prefix)
                .finish_non_exhaustive()
        }
    }

    impl S3Store {
        /// Connect from an `s3://bucket[/prefix]` URL.
        pub fn from_url(url: &str) -> Result<Self> {
            let rest = url
                .strip_prefix("s3://")
                .ok_or_else(|| bad(url, "expected s3://bucket[/prefix]"))?;
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            if bucket.is_empty() {
                return Err(bad(url, "missing bucket name"));
            }
            Self::connect(bucket, prefix)
        }

        /// Connect to `bucket`, keying everything under `prefix` (may be
        /// empty). Loads the AWS configuration; does not touch the bucket.
        pub fn connect(bucket: &str, prefix: &str) -> Result<Self> {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("open-tiles-s3")
                .enable_all()
                .build()
                .map_err(|e| bad(bucket, &format!("starting the S3 runtime: {e}")))?;
            let custom_endpoint = ["AWS_ENDPOINT_URL_S3", "AWS_ENDPOINT_URL"]
                .iter()
                .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()));
            let http = https_client()?;
            let client = block(rt.handle(), async move {
                let sdk = aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .http_client(http)
                    .load()
                    .await;
                let cfg = aws_sdk_s3::config::Builder::from(&sdk)
                    .force_path_style(custom_endpoint)
                    .build();
                Client::from_conf(cfg)
            });
            let prefix = prefix.trim_matches('/');
            let prefix = if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            };
            Ok(Self {
                rt: Some(rt),
                client,
                bucket: bucket.to_string(),
                prefix,
            })
        }

        /// The bucket name.
        pub fn bucket(&self) -> &str {
            &self.bucket
        }

        /// The full object key for a store key.
        pub fn object_key(&self, key: &str) -> String {
            format!("{}{}", self.prefix, key.trim_start_matches('/'))
        }

        /// `s3://bucket/full-key` — how a key appears in errors and logs.
        fn url(&self, key: &str) -> String {
            format!("s3://{}/{}", self.bucket, self.object_key(key))
        }

        /// Run one SDK call to completion from the calling (blocking) thread.
        fn run<T: Send + 'static>(&self, fut: impl Future<Output = T> + Send + 'static) -> T {
            block(
                self.rt.as_ref().expect("runtime lives until drop").handle(),
                fut,
            )
        }
    }

    impl Drop for S3Store {
        /// Dropping a `Runtime` inside an async context panics, and the
        /// store is owned by a `Config` the server drops from one;
        /// `shutdown_background` is allowed anywhere.
        fn drop(&mut self) {
            if let Some(rt) = self.rt.take() {
                rt.shutdown_background();
            }
        }
    }

    /// Run `fut` on the store's runtime and wait for it. Deliberately not
    /// `block_on`: the caller may be a `spawn_blocking` thread of the
    /// server's runtime, where entering another runtime panics. Spawning and
    /// waiting on a plain channel works from any thread.
    fn block<T: Send + 'static>(rt: &Handle, fut: impl Future<Output = T> + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        rt.spawn(async move {
            let _ = tx.send(fut.await);
        });
        rx.recv().expect("S3 task ended without a result")
    }

    /// The SDK's HTTPS client with Mozilla's root CAs bundled *in addition
    /// to* the platform's. The SDK default trusts platform roots only, which
    /// is empty in a slim container image and, behind a corporate proxy
    /// whose `SSL_CERT_FILE` names just its own CA, lacks Amazon's roots.
    /// Keeping the platform roots means such a CA is still honoured.
    fn https_client() -> Result<SharedHttpClient> {
        use aws_smithy_http_client::tls::{
            self, rustls_provider::CryptoMode, TlsContext, TrustStore,
        };
        let mut pem = String::new();
        for der in webpki_root_certs::TLS_SERVER_ROOT_CERTS {
            pem.push_str("-----BEGIN CERTIFICATE-----\n");
            let b64 = aws_smithy_types::base64::encode(der.as_ref());
            for line in b64.as_bytes().chunks(64) {
                pem.push_str(std::str::from_utf8(line).expect("base64 is ascii"));
                pem.push('\n');
            }
            pem.push_str("-----END CERTIFICATE-----\n");
        }
        // Only ask for platform roots when there are any: the SDK logs a
        // warning otherwise, which is noise in a slim container image.
        let native = !rustls_native_certs::load_native_certs().certs.is_empty();
        let trust = TrustStore::default()
            .with_native_roots(native)
            .with_pem_certificate(pem);
        let ctx = TlsContext::builder()
            .with_trust_store(trust)
            .build()
            .map_err(|e| bad("s3", &format!("building the TLS context: {e}")))?;
        Ok(aws_smithy_http_client::Builder::new()
            .tls_provider(tls::Provider::Rustls(CryptoMode::AwsLc))
            .tls_context(ctx)
            .build_https())
    }

    /// An [`Error::Io`] whose "path" is an S3 URL or bucket name.
    fn bad(what: &str, reason: &str) -> Error {
        Error::Io {
            path: what.to_string(),
            source: std::io::Error::other(reason.to_string()),
        }
    }

    /// A readable reason: `Code: message` for service errors, else the
    /// error chain (the SDK's `DisplayErrorContext` appends a Debug dump).
    fn sdk_err<E>(url: &str, e: SdkError<E, HttpResponse>) -> Error
    where
        E: ProvideErrorMetadata + std::error::Error + 'static,
    {
        let status = e.raw_response().map(|r| r.status().as_u16());
        let service = e.as_service_error();
        let reason = match (service.and_then(|s| s.code()), status) {
            (Some(code), _) => format!(
                "{code}: {}",
                service.and_then(|s| s.message()).unwrap_or("")
            ),
            // a HEAD error has no body, hence no code
            (None, Some(status)) if service.is_some() => format!("HTTP {status}"),
            _ => {
                let mut reason = e.to_string();
                let mut cur = std::error::Error::source(&e);
                while let Some(c) = cur {
                    reason.push_str(": ");
                    reason.push_str(&c.to_string());
                    cur = c.source();
                }
                reason
            }
        };
        bad(url, &reason)
    }

    /// Raw HTTP 404 — how a genuinely missing object (or bucket) comes back.
    fn is_404<E>(e: &SdkError<E, HttpResponse>) -> bool {
        e.raw_response().is_some_and(|r| r.status().as_u16() == 404)
    }

    /// Without `s3:ListBucket` on the bucket, S3 answers 403 instead of 404
    /// for a key that does not exist. Treat that as a miss — a genuinely
    /// unreadable bucket still surfaces on the `put` that follows — and say
    /// so once, because it also hides real permission problems on reads.
    fn is_missing<E: ProvideErrorMetadata>(e: &SdkError<E, HttpResponse>, bucket: &str) -> bool {
        if is_404(e) {
            return true;
        }
        // HEAD responses carry no body, so no error code: a bare 403 counts
        let denied = e.raw_response().is_some_and(|r| r.status().as_u16() == 403)
            && matches!(
                e.as_service_error().and_then(|s| s.code()),
                None | Some("AccessDenied")
            );
        if denied {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "s3://{bucket}: 403 AccessDenied when probing a key; treating it as a miss. \
                     Grant s3:ListBucket on the bucket so misses are 404s and read errors stay visible"
                );
            }
        }
        denied
    }

    /// Content type recorded on uploads, from the key's extension. Purely
    /// informational — readers sniff bytes — but it lets the bucket be
    /// browsed or fronted by a CDN sensibly.
    fn content_type(key: &str) -> &'static str {
        match key.rsplit('.').next() {
            Some("glb") => "model/gltf-binary",
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            _ => "application/octet-stream",
        }
    }

    impl Store for S3Store {
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
            let (client, bucket, k, url) = (
                self.client.clone(),
                self.bucket.clone(),
                self.object_key(key),
                self.url(key),
            );
            self.run(async move {
                let out = match client.get_object().bucket(&bucket).key(k).send().await {
                    Ok(out) => out,
                    Err(e) if is_missing(&e, &bucket) => return Ok(None),
                    Err(e) => return Err(sdk_err(&url, e)),
                };
                let body = out.body.collect().await.map_err(|e| {
                    bad(&url, &format!("reading body: {}", DisplayErrorContext(&e)))
                })?;
                Ok(Some(body.into_bytes().to_vec()))
            })
        }

        fn exists(&self, key: &str) -> Result<bool> {
            let (client, bucket, k, url) = (
                self.client.clone(),
                self.bucket.clone(),
                self.object_key(key),
                self.url(key),
            );
            self.run(async move {
                match client.head_object().bucket(&bucket).key(k).send().await {
                    Ok(_) => Ok(true),
                    Err(e) if is_missing(&e, &bucket) => Ok(false),
                    Err(e) => Err(sdk_err(&url, e)),
                }
            })
        }

        fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
            let (client, bucket, k, url) = (
                self.client.clone(),
                self.bucket.clone(),
                self.object_key(key),
                self.url(key),
            );
            let body = ByteStream::from(bytes.to_vec());
            let ct = content_type(key);
            self.run(async move {
                client
                    .put_object()
                    .bucket(bucket)
                    .key(k)
                    .content_type(ct)
                    .body(body)
                    .send()
                    .await
                    .map(|_| ())
                    .map_err(|e| sdk_err(&url, e))
            })
        }

        fn delete(&self, key: &str) -> Result<()> {
            let (client, bucket, k, url) = (
                self.client.clone(),
                self.bucket.clone(),
                self.object_key(key),
                self.url(key),
            );
            self.run(async move {
                // S3 answers 204 for a missing key; a 404 can only come from a
                // missing bucket or a stricter S3-compatible store — fine either way
                match client.delete_object().bucket(bucket).key(k).send().await {
                    Ok(_) => Ok(()),
                    Err(e) if is_404(&e) => Ok(()),
                    Err(e) => Err(sdk_err(&url, e)),
                }
            })
        }

        fn list(&self, prefix: &str) -> Result<Vec<String>> {
            let (client, bucket, strip) = (
                self.client.clone(),
                self.bucket.clone(),
                self.prefix.clone(),
            );
            let p = self.object_key(prefix);
            let url = self.url(prefix);
            self.run(async move {
                let mut keys = Vec::new();
                let mut pages = client
                    .list_objects_v2()
                    .bucket(bucket)
                    .prefix(p)
                    .into_paginator()
                    .send();
                while let Some(page) = pages.next().await {
                    let page = page.map_err(|e| sdk_err(&url, e))?;
                    for obj in page.contents() {
                        if let Some(k) = obj.key().and_then(|k| k.strip_prefix(strip.as_str())) {
                            keys.push(k.to_string());
                        }
                    }
                }
                keys.sort();
                Ok(keys)
            })
        }

        fn location(&self) -> String {
            format!("s3://{}/{}", self.bucket, self.prefix)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_store_round_trip_and_listing() {
        let dir = tempfile::tempdir().unwrap();
        let s = LocalStore::new(dir.path());
        assert_eq!(s.get("a/b/c.png").unwrap(), None);
        assert!(!s.exists("a/b/c.png").unwrap());
        s.put("a/b/c.png", b"one").unwrap();
        s.put("a/b/c.png.404", b"").unwrap();
        s.put("a/d.png", b"two").unwrap();
        assert_eq!(s.get("a/b/c.png").unwrap().as_deref(), Some(&b"one"[..]));
        assert!(s.exists("a/b/c.png.404").unwrap(), "empty entries exist");
        assert_eq!(
            s.list("a/").unwrap(),
            vec!["a/b/c.png", "a/b/c.png.404", "a/d.png"]
        );
        assert_eq!(s.list("a/b/c.png").unwrap(), vec!["a/b/c.png"]);
        assert_eq!(s.list("nope/").unwrap(), Vec::<String>::new());
        s.delete("a/b/c.png.404").unwrap();
        s.delete("a/b/c.png.404").unwrap(); // idempotent
        assert_eq!(s.list("a/b").unwrap(), vec!["a/b/c.png"]);
        assert_eq!(s.path("x//y/"), dir.path().join("x").join("y"));
        // no temp files left behind by the atomic writes
        assert_eq!(s.list("").unwrap(), vec!["a/b/c.png", "a/d.png"]);
    }

    #[test]
    fn open_picks_a_backend() {
        assert_eq!(open("/tmp/x").unwrap().location(), "/tmp/x");
    }

    #[cfg(feature = "s3")]
    #[test]
    fn s3_url_parsing() {
        // parsing is checked without a connection: bad URLs fail before that
        assert!(matches!(S3Store::from_url("s3://"), Err(Error::Io { .. })));
        assert!(matches!(
            S3Store::from_url("file:///x"),
            Err(Error::Io { .. })
        ));
    }
}
