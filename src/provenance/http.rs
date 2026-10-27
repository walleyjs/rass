use chrono::{DateTime, Utc};
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

use super::{AttestationEnvelope, AttestedPayload, AttestedPayloadProvider, ProvenanceError};

#[derive(Clone, Debug)]
struct CachedAttestation {
    payload: AttestedPayload,
    fetched_at: Instant,
}

/// Production HTTP provider that fetches live network responses, canonicalizes HTTP headers,
/// enforces rate-pacing & deduplication caching (for multiplexed event resolution),
/// and signs wire commitments with an Ed25519 authority key.
#[derive(Clone)]
pub struct HttpAttestingProvider {
    client: reqwest::Client,
    signing_key: Arc<SigningKey>,
    cache: Arc<RwLock<HashMap<String, CachedAttestation>>>,
    cache_ttl: Duration,
    min_request_interval: Duration,
    last_request_time: Arc<Mutex<Instant>>,
}

impl HttpAttestingProvider {
    pub fn new(cache_ttl: Duration, min_request_interval: Duration) -> Self {
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        Self::with_signing_key(signing_key, cache_ttl, min_request_interval)
    }

    pub fn with_signing_key(
        signing_key: SigningKey,
        cache_ttl: Duration,
        min_request_interval: Duration,
    ) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("ResolutionService-VerifiableOracle/1.0")
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            client,
            signing_key: Arc::new(signing_key),
            cache: Arc::new(RwLock::new(HashMap::new())),
            cache_ttl,
            min_request_interval,
            last_request_time: Arc::new(Mutex::new(Instant::now() - min_request_interval)),
        }
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    /// Fetches an ESPN summary payload with dual-domain failover (`site.web.api.espn.com` -> `site.api.espn.com`)
    /// and 120s in-cycle deduplication cache.
    pub async fn fetch_espn_summary_attested(
        &self,
        sport_path: &str,
        event_id: &str,
    ) -> Result<AttestedPayload, ProvenanceError> {
        let endpoints = [
            format!(
                "https://site.web.api.espn.com/apis/site/v2/sports/{}/summary?event={}",
                sport_path, event_id
            ),
            format!(
                "https://site.api.espn.com/apis/site/v2/sports/{}/summary?event={}",
                sport_path, event_id
            ),
        ];

        let mut last_err = ProvenanceError::NetworkError("No ESPN endpoints attempted".into());
        for url in &endpoints {
            match self.fetch_attested(url).await {
                Ok(attested) => return Ok(attested),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }
}

#[async_trait::async_trait]
impl AttestedPayloadProvider for HttpAttestingProvider {
    async fn fetch_attested(&self, url: &str) -> Result<AttestedPayload, ProvenanceError> {
        // 1. Check deduplication cache first
        {
            let reader = self.cache.read().await;
            if let Some(cached) = reader.get(url) {
                if cached.fetched_at.elapsed() < self.cache_ttl {
                    return Ok(cached.payload.clone());
                }
            }
        }

        // 2. Enforce rate pacing between outbound requests
        {
            let mut last = self.last_request_time.lock().await;
            let elapsed = last.elapsed();
            if elapsed < self.min_request_interval {
                tokio::time::sleep(self.min_request_interval - elapsed).await;
            }
            *last = Instant::now();
        }

        // 3. Execute live HTTPS request
        let resp = self
            .client
            .get(url)
            .header("Accept", "application/json, text/plain, */*")
            .send()
            .await
            .map_err(|e| ProvenanceError::NetworkError(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(ProvenanceError::NetworkError(format!(
                "HTTP status {} from {}",
                status, url
            )));
        }

        // 4. Extract canonical headers & server Date timestamp
        let tracked_headers = ["content-type", "date", "etag", "last-modified", "cache-control"];
        let mut canonical_headers = Vec::new();
        let mut server_date_ts: Option<i64> = None;

        for name in &tracked_headers {
            if let Some(val) = resp.headers().get(*name).and_then(|v| v.to_str().ok()) {
                if *name == "date" {
                    if let Ok(dt) = DateTime::parse_from_rfc2822(val) {
                        server_date_ts = Some(dt.with_timezone(&Utc).timestamp());
                    }
                }
                canonical_headers.push((name.to_string(), val.to_string()));
            }
        }
        canonical_headers.sort_by(|a, b| a.0.cmp(&b.0));

        let raw_bytes = resp
            .bytes()
            .await
            .map_err(|e| ProvenanceError::NetworkError(e.to_string()))?
            .to_vec();

        let mut hasher = Sha256::new();
        hasher.update(&raw_bytes);
        let payload_sha256: [u8; 32] = hasher.finalize().into();

        let fetch_timestamp = server_date_ts.unwrap_or_else(|| Utc::now().timestamp());
        let pubkey = self.public_key_bytes();

        let mut attested = AttestedPayload {
            raw_bytes,
            payload_sha256,
            canonical_headers,
            source_url: url.to_string(),
            fetch_timestamp,
            rfc3161_timestamp_token: None,
            server_tls_cert_sha256: None,
            attestation: AttestationEnvelope::Ed25519Authority {
                authority_pubkey: pubkey,
                signature: Vec::new(),
            },
        };

        let commitment = attested.commitment_hash();
        let sig = self.signing_key.sign(&commitment);
        attested.attestation = AttestationEnvelope::Ed25519Authority {
            authority_pubkey: pubkey,
            signature: sig.to_bytes().to_vec(),
        };

        // 5. Store in deduplication cache
        {
            let mut writer = self.cache.write().await;
            writer.insert(
                url.to_string(),
                CachedAttestation {
                    payload: attested.clone(),
                    fetched_at: Instant::now(),
                },
            );
        }

        Ok(attested)
    }
}
