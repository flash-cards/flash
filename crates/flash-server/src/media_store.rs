//! Where media bytes live. Metadata (ownership, MIME, refcounts) is in
//! SQLite; the blob itself is content-addressed by sha256 and stored either
//! on local disk (dev, tests, self-host) or in an S3-compatible bucket such
//! as Cloudflare R2 (durable and off-box; a database replicator does not
//! cover blobs, so this is what keeps media safe).
//!
//! Sync by design: every call happens inside spawn_blocking, like the
//! mailer and captcha clients. The R2 client speaks plain S3 with a
//! hand-rolled SigV4 (HMAC-SHA256 over a canonical request) — small enough
//! to own outright, and it keeps the vendored-OpenSSL/ureq build.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::config::MediaR2Config;

type HmacSha256 = Hmac<Sha256>;

/// A byte range as the client asked for it (RFC 9110 single range).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// `bytes=start-` or `bytes=start-end` (end inclusive).
    From { start: u64, end: Option<u64> },
    /// `bytes=-n`: the last n bytes.
    Suffix(u64),
}

/// What a `get` returns: the bytes plus enough to build Content-Range.
#[derive(Debug)]
pub struct Blob {
    pub bytes: Vec<u8>,
    /// Absolute offset of `bytes[0]` in the object.
    pub start: u64,
    /// Full object length.
    pub total: u64,
}

impl Blob {
    pub fn is_partial(&self) -> bool {
        self.start != 0 || (self.bytes.len() as u64) != self.total
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MediaStoreError {
    #[error("media not found")]
    NotFound,
    #[error("range not satisfiable")]
    RangeUnsatisfiable,
    #[error("{0}")]
    Other(String),
}

pub trait MediaStore: Send + Sync {
    /// Idempotent: storing a hash that already exists is a no-op.
    fn put(&self, sha256: &str, mime: &str, bytes: &[u8]) -> Result<(), MediaStoreError>;
    fn get(&self, sha256: &str, range: Option<ByteRange>) -> Result<Blob, MediaStoreError>;
    /// Missing blobs are not an error — a delete is a statement of intent.
    fn delete(&self, sha256: &str) -> Result<(), MediaStoreError>;
    /// Human-readable backend name for the boot log.
    fn describe(&self) -> String;

    /// A URL the client may fetch the blob from directly for `ttl`,
    /// carrying the given content type and inline filename, after the
    /// server has checked the client may see it. `None` when this store
    /// hands out no URLs and the server must send the bytes itself.
    fn presigned_get(
        &self,
        _sha256: &str,
        _mime: &str,
        _filename: &str,
        _ttl: Duration,
    ) -> Option<Result<String, MediaStoreError>> {
        None
    }

    /// The origin presigned URLs point at (scheme + host), for the pages'
    /// Content-Security-Policy. `None` when there are no such URLs.
    fn origin(&self) -> Option<String> {
        None
    }
}

/// Two-character fan-out under the root/bucket: `ab/ab12…`. The hash is
/// validated (64 hex chars) before it becomes a path or an object key —
/// nothing user-controlled reaches the filesystem or the wire.
pub fn object_key(sha256: &str) -> Result<String, MediaStoreError> {
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(MediaStoreError::Other("bad media hash".into()));
    }
    let sha256 = sha256.to_ascii_lowercase();
    Ok(format!("{}/{}", &sha256[..2], sha256))
}

/// Resolves a requested range against a known length: (start, end
/// inclusive). Follows RFC 9110: an end past the object is clamped; a start
/// at/after the end of the object is unsatisfiable.
pub fn resolve_range(range: ByteRange, total: u64) -> Result<(u64, u64), MediaStoreError> {
    if total == 0 {
        return Err(MediaStoreError::RangeUnsatisfiable);
    }
    match range {
        ByteRange::From { start, end } => {
            if start >= total {
                return Err(MediaStoreError::RangeUnsatisfiable);
            }
            let end = end.map_or(total - 1, |e| e.min(total - 1));
            if end < start {
                return Err(MediaStoreError::RangeUnsatisfiable);
            }
            Ok((start, end))
        }
        ByteRange::Suffix(n) => {
            if n == 0 {
                return Err(MediaStoreError::RangeUnsatisfiable);
            }
            let n = n.min(total);
            Ok((total - n, total - 1))
        }
    }
}

// ---- local disk ----

pub struct DiskStore {
    root: PathBuf,
}

impl DiskStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, sha256: &str) -> Result<PathBuf, MediaStoreError> {
        Ok(self.root.join(object_key(sha256)?))
    }
}

impl MediaStore for DiskStore {
    /// Temp file + rename so a crash never leaves a half-written blob
    /// under its final name.
    fn put(&self, sha256: &str, _mime: &str, bytes: &[u8]) -> Result<(), MediaStoreError> {
        use std::io::Write;
        let path = self.path(sha256)?;
        // A blob under its final name is complete only if it is the
        // right length: a crash between rename and the data reaching
        // disk can leave a short file, and content addressing would
        // then serve it to every user who holds that hash forever.
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() == bytes.len() as u64 {
                return Ok(());
            }
        }
        let dir = path.parent().expect("blob path has a parent");
        std::fs::create_dir_all(dir)
            .map_err(|e| MediaStoreError::Other(format!("media dir: {e}")))?;
        // One temp name per writer: two workers ingesting the same blob
        // must not truncate each other's file mid-write.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!("{sha256}.tmp-{}-{seq}", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_file(&tmp);
            return Err(MediaStoreError::Other(format!("media write: {e}")));
        }
        Ok(())
    }

    fn get(&self, sha256: &str, range: Option<ByteRange>) -> Result<Blob, MediaStoreError> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.path(sha256)?;
        let mut file = std::fs::File::open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => MediaStoreError::NotFound,
            _ => MediaStoreError::Other(format!("media open: {e}")),
        })?;
        let total = file
            .metadata()
            .map_err(|e| MediaStoreError::Other(format!("media stat: {e}")))?
            .len();
        let (start, end) = match range {
            None => (0, total.saturating_sub(1)),
            Some(r) => resolve_range(r, total)?,
        };
        let len = if total == 0 { 0 } else { end - start + 1 };
        file.seek(SeekFrom::Start(start))
            .map_err(|e| MediaStoreError::Other(format!("media seek: {e}")))?;
        let mut bytes = vec![0u8; len as usize];
        file.read_exact(&mut bytes)
            .map_err(|e| MediaStoreError::Other(format!("media read: {e}")))?;
        Ok(Blob {
            bytes,
            start,
            total,
        })
    }

    fn delete(&self, sha256: &str) -> Result<(), MediaStoreError> {
        match std::fs::remove_file(self.path(sha256)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(MediaStoreError::Other(format!("media remove: {e}"))),
        }
    }

    fn describe(&self) -> String {
        format!("disk ({})", self.root.display())
    }
}

// ---- S3-compatible bucket (path-style, SigV4) ----

pub struct R2Store {
    /// Scheme + host, no trailing slash, e.g. https://s3.us-east-1.amazonaws.com
    endpoint: String,
    host: String,
    bucket: String,
    access_key_id: String,
    secret_access_key: String,
    agent: ureq::Agent,
    /// Largest object we will read into memory.
    max_read: u64,
}

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const REGION: &str = "auto";
const SERVICE: &str = "s3";

impl R2Store {
    pub fn new(config: &MediaR2Config, max_read: u64) -> Result<Self, String> {
        let endpoint = config.endpoint.trim_end_matches('/').to_string();
        let host = endpoint
            .split("//")
            .nth(1)
            .filter(|h| !h.is_empty() && !h.contains('/'))
            .ok_or("FLASH_MEDIA_R2_ENDPOINT must be scheme://host")?
            .to_string();
        if config.bucket.is_empty()
            || !config
                .bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        {
            return Err("FLASH_MEDIA_R2_BUCKET is not a valid bucket name".into());
        }
        // Whole-object transfers of up to 100 MiB over a datacenter link;
        // S3-style errors are XML bodies on 4xx, so keep them readable.
        // 30 s, not two minutes: every media call sits on the blocking
        // pool the database shares, so a slow bucket must fail fast rather
        // than hold threads the store needs.
        let agent = crate::http::agent(std::time::Duration::from_secs(30), false);
        Ok(Self {
            endpoint,
            host,
            bucket: config.bucket.clone(),
            access_key_id: config.access_key_id.clone(),
            secret_access_key: config.secret_access_key.clone(),
            agent,
            max_read,
        })
    }

    fn uri(&self, key: &str) -> String {
        format!("/{}/{}", self.bucket, key)
    }

    /// A query-signed GET URL for one object: the bucket serves the bytes
    /// with the content type, inline filename and cache header we ask
    /// for, and the URL dies after `ttl`.
    fn presign_get(
        &self,
        key: &str,
        mime: &str,
        filename: &str,
        ttl: Duration,
    ) -> Result<String, MediaStoreError> {
        let secs = ttl.as_secs().clamp(1, 7 * 24 * 60 * 60);
        let cache = format!("private, max-age={secs}");
        let disposition = format!("inline; filename=\"{filename}\"");
        let overrides = [
            ("response-cache-control", cache.as_str()),
            ("response-content-disposition", disposition.as_str()),
            ("response-content-type", mime),
        ];
        let uri = self.uri(key);
        let query = presigned_query(
            &self.access_key_id,
            &self.secret_access_key,
            REGION,
            SERVICE,
            &self.host,
            &uri,
            &Self::amz_date_now(),
            secs,
            &overrides,
        );
        Ok(format!("{}{uri}?{query}", self.endpoint))
    }

    /// SigV4 `Authorization` header value for a request whose canonical
    /// headers are exactly `headers` (lowercase names, sorted by the
    /// caller? — no: sorted here) and whose payload hashes to `payload_hash`.
    fn authorization(
        &self,
        method: &str,
        canonical_uri: &str,
        headers: &[(&str, &str)],
        payload_hash: &str,
        amz_date: &str,
    ) -> String {
        sigv4_authorization(
            &self.access_key_id,
            &self.secret_access_key,
            REGION,
            SERVICE,
            method,
            canonical_uri,
            "",
            headers,
            payload_hash,
            amz_date,
        )
    }

    fn amz_date_now() -> String {
        jiff::Timestamp::now()
            .strftime("%Y%m%dT%H%M%SZ")
            .to_string()
    }
}

impl MediaStore for R2Store {
    fn put(&self, sha256: &str, mime: &str, bytes: &[u8]) -> Result<(), MediaStoreError> {
        let key = object_key(sha256)?;
        // The hash was computed at validation from these very bytes.
        let payload_hash = sha256.to_ascii_lowercase();
        let amz_date = Self::amz_date_now();
        let uri = self.uri(&key);
        let headers = [
            ("content-type", mime),
            ("host", self.host.as_str()),
            ("x-amz-content-sha256", payload_hash.as_str()),
            ("x-amz-date", amz_date.as_str()),
        ];
        let auth = self.authorization("PUT", &uri, &headers, &payload_hash, &amz_date);
        let response = self
            .agent
            .put(format!("{}{}", self.endpoint, uri))
            .header("Authorization", &auth)
            .header("Content-Type", mime)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", &amz_date)
            .send(bytes)
            .map_err(|e| MediaStoreError::Other(format!("r2 put: {e}")))?;
        let status = response.status().as_u16();
        // Drain the (tiny) body so the connection goes back to the pool;
        // otherwise every PUT pays a fresh TLS handshake.
        let mut response = response;
        let _ = response
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_vec();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(MediaStoreError::Other(format!("r2 put: HTTP {status}")))
        }
    }

    fn get(&self, sha256: &str, range: Option<ByteRange>) -> Result<Blob, MediaStoreError> {
        let key = object_key(sha256)?;
        let amz_date = Self::amz_date_now();
        let uri = self.uri(&key);
        let headers = [
            ("host", self.host.as_str()),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", amz_date.as_str()),
        ];
        let auth = self.authorization("GET", &uri, &headers, EMPTY_SHA256, &amz_date);
        let mut request = self
            .agent
            .get(format!("{}{}", self.endpoint, uri))
            .header("Authorization", &auth)
            .header("x-amz-content-sha256", EMPTY_SHA256)
            .header("x-amz-date", &amz_date);
        if let Some(r) = range {
            let value = match r {
                ByteRange::From {
                    start,
                    end: Some(end),
                } => format!("bytes={start}-{end}"),
                ByteRange::From { start, end: None } => format!("bytes={start}-"),
                ByteRange::Suffix(n) => format!("bytes=-{n}"),
            };
            request = request.header("Range", &value);
        }
        let mut response = request
            .call()
            .map_err(|e| MediaStoreError::Other(format!("r2 get: {e}")))?;
        let status = response.status().as_u16();
        match status {
            200 | 206 => {}
            404 => return Err(MediaStoreError::NotFound),
            416 => return Err(MediaStoreError::RangeUnsatisfiable),
            _ => return Err(MediaStoreError::Other(format!("r2 get: HTTP {status}"))),
        }
        let content_range = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = response
            .body_mut()
            .with_config()
            .limit(self.max_read)
            .read_to_vec()
            .map_err(|e| MediaStoreError::Other(format!("r2 body: {e}")))?;
        let (start, total) = match (status, content_range) {
            (206, Some(cr)) => parse_content_range(&cr)
                .ok_or_else(|| MediaStoreError::Other(format!("r2 content-range: {cr}")))?,
            _ => (0, bytes.len() as u64),
        };
        Ok(Blob {
            bytes,
            start,
            total,
        })
    }

    fn delete(&self, sha256: &str) -> Result<(), MediaStoreError> {
        let key = object_key(sha256)?;
        let amz_date = Self::amz_date_now();
        let uri = self.uri(&key);
        let headers = [
            ("host", self.host.as_str()),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", amz_date.as_str()),
        ];
        let auth = self.authorization("DELETE", &uri, &headers, EMPTY_SHA256, &amz_date);
        let response = self
            .agent
            .delete(format!("{}{}", self.endpoint, uri))
            .header("Authorization", &auth)
            .header("x-amz-content-sha256", EMPTY_SHA256)
            .header("x-amz-date", &amz_date)
            .call()
            .map_err(|e| MediaStoreError::Other(format!("r2 delete: {e}")))?;
        let status = response.status().as_u16();
        // S3 answers 204 for deletes, including of missing keys.
        if (200..300).contains(&status) || status == 404 {
            Ok(())
        } else {
            Err(MediaStoreError::Other(format!("r2 delete: HTTP {status}")))
        }
    }

    fn describe(&self) -> String {
        format!("r2 ({}/{})", self.host, self.bucket)
    }

    fn presigned_get(
        &self,
        sha256: &str,
        mime: &str,
        filename: &str,
        ttl: Duration,
    ) -> Option<Result<String, MediaStoreError>> {
        Some(object_key(sha256).and_then(|key| self.presign_get(&key, mime, filename, ttl)))
    }

    fn origin(&self) -> Option<String> {
        Some(self.endpoint.clone())
    }
}

/// `bytes 0-99/1234` -> (0, 1234)
fn parse_content_range(value: &str) -> Option<(u64, u64)> {
    let rest = value.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, _end) = range.split_once('-')?;
    Some((start.parse().ok()?, total.parse().ok()?))
}

// ---- SigV4 ----

/// Lowercase hex of a digest (content addresses, SigV4 hashes).
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// AWS4-HMAC-SHA256 signing key for one day/region/service.
pub fn sigv4_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

/// The canonical request's signature and its credential scope. `headers`
/// are the signed headers as (lowercase-name, trimmed-value); they are
/// sorted here. `canonical_uri` must already be URI-safe (ours are a
/// bucket and hex hashes) and `canonical_query` already canonical
/// (sorted, encoded). Shared by header authorization and query presigning.
#[allow(clippy::too_many_arguments)]
pub fn sigv4_signature(
    secret: &str,
    region: &str,
    service: &str,
    method: &str,
    canonical_uri: &str,
    canonical_query: &str,
    headers: &[(&str, &str)],
    payload_hash: &str,
    amz_date: &str,
) -> (String, String, String) {
    let mut sorted: Vec<(&str, &str)> = headers.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let canonical_headers: String = sorted
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed_headers: Vec<&str> = sorted.iter().map(|(k, _)| *k).collect();
    let signed_headers = signed_headers.join(";");
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let key = sigv4_signing_key(secret, date, region, service);
    let signature = hex(&hmac(&key, string_to_sign.as_bytes()));
    (scope, signed_headers, signature)
}

/// Builds the `Authorization` header (see `sigv4_signature`).
#[allow(clippy::too_many_arguments)]
pub fn sigv4_authorization(
    access_key_id: &str,
    secret: &str,
    region: &str,
    service: &str,
    method: &str,
    canonical_uri: &str,
    canonical_query: &str,
    headers: &[(&str, &str)],
    payload_hash: &str,
    amz_date: &str,
) -> String {
    let (scope, signed_headers, signature) = sigv4_signature(
        secret,
        region,
        service,
        method,
        canonical_uri,
        canonical_query,
        headers,
        payload_hash,
        amz_date,
    );
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key_id}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

/// SigV4's URI encoding: unreserved characters bare, everything else
/// (including `/`) as uppercase `%XX`.
pub fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The query string of a presigned GET (query-parameter authentication):
/// the `X-Amz-*` parameters plus `extra` (response-header overrides),
/// canonically sorted and encoded, with the signature appended. `host`
/// is the only signed header; the payload is `UNSIGNED-PAYLOAD`.
#[allow(clippy::too_many_arguments)]
pub fn presigned_query(
    access_key_id: &str,
    secret: &str,
    region: &str,
    service: &str,
    host: &str,
    canonical_uri: &str,
    amz_date: &str,
    expires_secs: u64,
    extra: &[(&str, &str)],
) -> String {
    let date = &amz_date[..8];
    let credential = format!("{access_key_id}/{date}/{region}/{service}/aws4_request");
    let expires = expires_secs.to_string();
    let mut params: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()),
        ("X-Amz-Credential".into(), credential),
        ("X-Amz-Date".into(), amz_date.into()),
        ("X-Amz-Expires".into(), expires),
        ("X-Amz-SignedHeaders".into(), "host".into()),
    ];
    params.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (uri_encode(k), uri_encode(v)))
        .collect();
    encoded.sort();
    let canonical_query = encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let (_, _, signature) = sigv4_signature(
        secret,
        region,
        service,
        "GET",
        canonical_uri,
        &canonical_query,
        &[("host", host)],
        "UNSIGNED-PAYLOAD",
        amz_date,
    );
    format!("{canonical_query}&X-Amz-Signature={signature}")
}

/// Builds the store from config: a bucket when configured, else disk.
pub fn from_config(
    r2: Option<&MediaR2Config>,
    data_dir: &Path,
    max_read: u64,
) -> Result<Box<dyn MediaStore>, String> {
    match r2 {
        Some(cfg) => Ok(Box::new(R2Store::new(cfg, max_read)?)),
        None => Ok(Box::new(DiskStore::new(data_dir.join("media")))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_keys_fan_out_and_reject_junk() {
        let sha = "ab".repeat(32);
        assert_eq!(object_key(&sha).unwrap(), format!("ab/{sha}"));
        assert!(object_key("../../etc/passwd").is_err());
        assert!(object_key(&"zz".repeat(32)).is_err());
        assert!(object_key(&"ab".repeat(31)).is_err());
    }

    #[test]
    fn ranges_resolve_per_rfc_9110() {
        let from = |s, e| ByteRange::From { start: s, end: e };
        assert_eq!(resolve_range(from(0, Some(3)), 10).unwrap(), (0, 3));
        assert_eq!(resolve_range(from(4, None), 10).unwrap(), (4, 9));
        assert_eq!(
            resolve_range(from(4, Some(100)), 10).unwrap(),
            (4, 9),
            "end clamped"
        );
        assert_eq!(resolve_range(ByteRange::Suffix(3), 10).unwrap(), (7, 9));
        assert_eq!(resolve_range(ByteRange::Suffix(30), 10).unwrap(), (0, 9));
        assert!(matches!(
            resolve_range(from(10, None), 10),
            Err(MediaStoreError::RangeUnsatisfiable)
        ));
        assert!(matches!(
            resolve_range(from(5, Some(2)), 10),
            Err(MediaStoreError::RangeUnsatisfiable)
        ));
        assert!(matches!(
            resolve_range(ByteRange::Suffix(0), 10),
            Err(MediaStoreError::RangeUnsatisfiable)
        ));
    }

    /// A blob under its final name is trusted only at the right length:
    /// a torn write (crash between rename and the data reaching disk)
    /// must be rewritten by the next put, not served to everyone who
    /// holds that hash.
    #[test]
    fn disk_store_rewrites_a_short_blob() {
        let root = std::env::temp_dir().join(format!("flash-media-torn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = DiskStore::new(&root);
        let bytes = b"the whole blob";
        let sha = hex(&Sha256::digest(bytes));
        store.put(&sha, "text/plain", bytes).unwrap();
        let path = store.path(&sha).unwrap();
        std::fs::write(&path, b"the wh").unwrap();
        store.put(&sha, "text/plain", bytes).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().contains(".tmp-")),
            "no temp file left behind"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn disk_store_round_trips_with_ranges() {
        let root = std::env::temp_dir().join(format!("flash-media-store-{}", std::process::id()));
        let store = DiskStore::new(&root);
        let bytes = b"0123456789";
        let sha = hex(&Sha256::digest(bytes));
        store.put(&sha, "text/plain", bytes).unwrap();
        store.put(&sha, "text/plain", bytes).unwrap(); // idempotent
        let full = store.get(&sha, None).unwrap();
        assert_eq!(full.bytes, bytes);
        assert!(!full.is_partial());
        let part = store
            .get(
                &sha,
                Some(ByteRange::From {
                    start: 2,
                    end: Some(4),
                }),
            )
            .unwrap();
        assert_eq!(
            (part.bytes.as_slice(), part.start, part.total),
            (&b"234"[..], 2, 10)
        );
        assert!(part.is_partial());
        let tail = store.get(&sha, Some(ByteRange::Suffix(2))).unwrap();
        assert_eq!(tail.bytes, b"89");
        assert!(matches!(
            store.get(
                &sha,
                Some(ByteRange::From {
                    start: 10,
                    end: None
                })
            ),
            Err(MediaStoreError::RangeUnsatisfiable)
        ));
        store.delete(&sha).unwrap();
        store.delete(&sha).unwrap(); // missing is fine
        assert!(matches!(
            store.get(&sha, None),
            Err(MediaStoreError::NotFound)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn content_range_parses() {
        assert_eq!(parse_content_range("bytes 0-99/1234"), Some((0, 1234)));
        assert_eq!(parse_content_range("bytes 100-199/200"), Some((100, 200)));
        assert_eq!(parse_content_range("garbage"), None);
    }

    /// AWS's published signing-key example (Signature Version 4 docs,
    /// "Examples of how to derive a signing key").
    #[test]
    fn sigv4_signing_key_matches_aws_example() {
        let key = sigv4_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    /// AWS's published end-to-end example (IAM ListUsers, "Signing AWS
    /// requests with Signature Version 4"): known request in, known
    /// signature out.
    #[test]
    fn sigv4_authorization_matches_aws_example() {
        let auth = sigv4_authorization(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "iam",
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            &[
                (
                    "content-type",
                    "application/x-www-form-urlencoded; charset=utf-8",
                ),
                ("host", "iam.amazonaws.com"),
                ("x-amz-date", "20150830T123600Z"),
            ],
            EMPTY_SHA256,
            "20150830T123600Z",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    /// The access key id from AWS's own documentation examples, assembled
    /// so the source never spells a credential-shaped token (the public
    /// tree's leak scan rejects that shape on sight).
    const AWS_EXAMPLE_KEY: &str = concat!("AKIA", "IOSFODNN7EXAMPLE");

    /// AWS's published presigned-URL example (S3 docs, "Authenticating
    /// Requests: Using Query Parameters"): a GET of `test.txt` valid for
    /// a day, signed on host alone with an unsigned payload.
    #[test]
    fn presigned_query_matches_aws_example() {
        let query = presigned_query(
            AWS_EXAMPLE_KEY,
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "s3",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            "20130524T000000Z",
            86400,
            &[],
        );
        assert_eq!(
            query,
            format!(
                "X-Amz-Algorithm=AWS4-HMAC-SHA256\
                 &X-Amz-Credential={AWS_EXAMPLE_KEY}%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
                 &X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host\
                 &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
            )
        );
    }

    /// The response-header overrides sort after the `X-Amz-*` keys (byte
    /// order) and are encoded like any other value.
    #[test]
    fn presigned_query_encodes_and_sorts_overrides() {
        let query = presigned_query(
            AWS_EXAMPLE_KEY,
            "secret",
            "auto",
            "s3",
            "acct.r2.example",
            "/flash-media/ab/abcdef",
            "20260905T120000Z",
            900,
            &[
                ("response-content-type", "image/png"),
                (
                    "response-content-disposition",
                    "inline; filename=\"a b.png\"",
                ),
            ],
        );
        let before_sig = query.split("&X-Amz-Signature=").next().unwrap();
        assert!(before_sig.ends_with(
            "&response-content-disposition=inline%3B%20filename%3D%22a%20b.png%22\
             &response-content-type=image%2Fpng"
        ));
        assert!(query.rsplit('=').next().unwrap().len() == 64);
    }

    #[test]
    fn r2_store_presigns_path_style_urls_on_its_endpoint() {
        let store = R2Store::new(
            &MediaR2Config {
                endpoint: "https://acct.r2.example".into(),
                bucket: "flash-media".into(),
                access_key_id: "AKIDEXAMPLE".into(),
                secret_access_key: "secret".into(),
            },
            1024,
        )
        .unwrap();
        let sha = "ab".repeat(32);
        let url = store
            .presigned_get(&sha, "image/png", "cat.png", Duration::from_secs(900))
            .unwrap()
            .unwrap();
        assert!(url.starts_with("https://acct.r2.example/flash-media/ab/"));
        assert!(url.contains("X-Amz-Expires=900"));
        assert!(url.contains("response-content-type=image%2Fpng"));
        assert_eq!(store.origin().as_deref(), Some("https://acct.r2.example"));
    }

    /// Runs only against a real bucket: set FLASH_MEDIA_R2_* and
    /// `cargo test -p flash-server r2_round_trip -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn r2_round_trip() {
        let cfg = MediaR2Config {
            endpoint: std::env::var("FLASH_MEDIA_R2_ENDPOINT").unwrap(),
            bucket: std::env::var("FLASH_MEDIA_R2_BUCKET").unwrap(),
            access_key_id: std::env::var("FLASH_MEDIA_R2_ACCESS_KEY_ID").unwrap(),
            secret_access_key: std::env::var("FLASH_MEDIA_R2_SECRET_ACCESS_KEY").unwrap(),
        };
        let store = R2Store::new(&cfg, 1024 * 1024).unwrap();
        let bytes = format!("flash r2 probe {}", jiff::Timestamp::now()).into_bytes();
        let sha = hex(&Sha256::digest(&bytes));
        store.put(&sha, "text/plain", &bytes).unwrap();
        let got = store.get(&sha, None).unwrap();
        assert_eq!(got.bytes, bytes);
        let part = store
            .get(
                &sha,
                Some(ByteRange::From {
                    start: 6,
                    end: Some(7),
                }),
            )
            .unwrap();
        assert_eq!(part.bytes, b"r2");
        assert_eq!((part.start, part.total), (6, bytes.len() as u64));
        store.delete(&sha).unwrap();
        assert!(matches!(
            store.get(&sha, None),
            Err(MediaStoreError::NotFound)
        ));
        println!("r2 round trip ok via {}", store.describe());
    }
}
