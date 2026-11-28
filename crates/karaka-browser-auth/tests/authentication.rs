//! Browser credentials. (Karaka: `browser-auth/tests/authentication.spec.ts`.)
//!
//! Tokens are signed here with keys ring makes; the RS256 case uses a token
//! signed once, offline, whose private key was not kept (`fixtures/README`).
//! Not ported: "propagates verification infrastructure errors", which mocks
//! jose's `jwtVerify`; verification here makes no error but jose's.

use std::time::{SystemTime, UNIX_EPOCH};

use karaka_browser_auth::{
    Algorithm, BrowserAuthConfig, BrowserAuthPlugin, BrowserAuthentication, JoseError, KeyConfig,
    encode_standard, encode_url,
};
use karaka_kernel::Runtime;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};

/// An SPKI PEM of `der`.
fn pem(der: &[u8]) -> String {
    let body = encode_standard(der);
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect();
    format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        lines.join("\n")
    )
}

/// The P-256 SPKI prefix before the uncompressed point.
const P256_SPKI: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// The Ed25519 SPKI prefix before the key.
const ED25519_SPKI: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

struct Signer {
    key: EcdsaKeyPair,
    public: String,
}

fn es256() -> Signer {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let key =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    let mut der = P256_SPKI.to_vec();
    der.extend_from_slice(key.public_key().as_ref());
    Signer {
        public: pem(&der),
        key,
    }
}

impl Signer {
    fn sign(&self, header: &Value, claims: &Value) -> String {
        let message = format!(
            "{}.{}",
            encode_url(header.to_string().as_bytes()),
            encode_url(claims.to_string().as_bytes())
        );
        let signature = self
            .key
            .sign(&SystemRandom::new(), message.as_bytes())
            .unwrap();
        format!("{message}.{}", encode_url(signature.as_ref()))
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn config(keys: Vec<KeyConfig>) -> BrowserAuthConfig {
    BrowserAuthConfig {
        application_id: "app".into(),
        issuer: "issuer".into(),
        audience: "karaka".into(),
        max_token_age_seconds: 300,
        keys,
    }
}

fn key(id: &str, algorithm: Algorithm, public_key: &str) -> KeyConfig {
    KeyConfig {
        id: id.into(),
        algorithm,
        public_key: public_key.into(),
    }
}

/// Fresh claims with `overrides`; `"<drop>"` removes a claim.
fn claims(overrides: Value) -> Value {
    let now = now();
    let mut claims = json!({
        "applicationId": "app", "tenantId": "tenant", "userId": "user", "chatId": "chat",
        "iss": "issuer", "aud": "karaka", "iat": now, "exp": now + 120,
    });
    for (name, value) in overrides.as_object().unwrap() {
        if value == "<drop>" {
            claims.as_object_mut().unwrap().remove(name);
        } else {
            claims[name] = value.clone();
        }
    }
    claims
}

fn header(kid: &str) -> Value {
    json!({ "alg": "ES256", "kid": kid })
}

fn verifier(signer: &Signer) -> BrowserAuthentication {
    BrowserAuthentication::new(config(vec![key("key", Algorithm::Es256, &signer.public)])).unwrap()
}

#[tokio::test]
async fn the_signed_owner_chat_and_expiry_come_back() {
    let signer = es256();
    let runtime = Runtime::start(vec![Box::new(BrowserAuthPlugin::new(config(vec![key(
        "key",
        Algorithm::Es256,
        &signer.public,
    )])))])
    .await
    .unwrap();
    let auth = runtime.get::<BrowserAuthentication>();
    let exp = now() + 120;
    let caller = auth
        .authenticate(&signer.sign(&header("key"), &claims(json!({ "exp": exp }))))
        .unwrap();
    assert_eq!(caller.owner.application_id.as_str(), "app");
    assert_eq!(caller.owner.tenant_id.as_str(), "tenant");
    assert_eq!(caller.owner.user_id.as_str(), "user");
    assert_eq!(caller.chat_id.as_str(), "chat");
    assert_eq!(caller.expires_at, exp * 1000);
}

#[test]
fn every_bad_claim_is_denied() {
    let signer = es256();
    let auth = verifier(&signer);
    for (label, overrides) in [
        ("foreign application", json!({ "applicationId": "another" })),
        ("foreign issuer", json!({ "iss": "another" })),
        ("foreign audience", json!({ "aud": "another" })),
        ("empty tenant", json!({ "tenantId": "" })),
        ("non-string user", json!({ "userId": 1 })),
        ("missing owner claim", json!({ "userId": "<drop>" })),
        ("missing chat claim", json!({ "chatId": "<drop>" })),
        ("empty chat claim", json!({ "chatId": "" })),
        ("non-string chat claim", json!({ "chatId": 1 })),
        ("null chat claim", json!({ "chatId": null })),
        ("expired credential", json!({ "iat": 1, "exp": 2 })),
        (
            "excessive issued lifetime",
            json!({ "iat": 1, "exp": 4_102_444_800_u64 }),
        ),
        (
            "future issuance",
            json!({ "iat": 4_102_444_000_u64, "exp": 4_102_444_100_u64 }),
        ),
        ("future not-before", json!({ "nbf": 4_102_444_000_u64 })),
        ("non-number issuance", json!({ "iat": "now" })),
    ] {
        let claims = claims(overrides);
        assert_eq!(
            auth.authenticate(&signer.sign(&header("key"), &claims)),
            None,
            "{label}"
        );
    }
    // An audience array holding this audience passes.
    let listed = claims(json!({ "aud": ["other", "karaka"] }));
    assert!(
        auth.authenticate(&signer.sign(&header("key"), &listed))
            .is_some()
    );
}

#[test]
fn the_exact_signed_chat_is_kept() {
    let signer = es256();
    let auth = verifier(&signer);
    let other = signer.sign(&header("key"), &claims(json!({ "chatId": "another-chat" })));
    assert_eq!(
        auth.authenticate(&other).unwrap().chat_id.as_str(),
        "another-chat"
    );
    let first = signer.sign(&header("key"), &claims(json!({})));
    assert_eq!(auth.authenticate(&first).unwrap().chat_id.as_str(), "chat");
}

#[test]
fn malformed_tokens_unknown_keys_and_other_keys_signatures_are_denied() {
    let signer = es256();
    let auth = verifier(&signer);
    let at = now();
    assert_eq!(
        auth.check("not-a-jwt", at),
        Err(Some(JoseError::JwsInvalid))
    );
    let unknown = signer.sign(&header("unknown"), &claims(json!({})));
    assert_eq!(
        auth.check(&unknown, at),
        Err(Some(JoseError::SignatureVerificationFailed))
    );
    let forged = es256().sign(&header("key"), &json!({}));
    assert_eq!(
        auth.check(&forged, at),
        Err(Some(JoseError::SignatureVerificationFailed))
    );
    // No protected key id.
    let anonymous = signer.sign(&json!({ "alg": "ES256" }), &claims(json!({})));
    assert_eq!(auth.authenticate(&anonymous), None);
    // An algorithm no key has.
    let none = signer.sign(&json!({ "alg": "none", "kid": "key" }), &claims(json!({})));
    assert_eq!(auth.check(&none, at), Err(Some(JoseError::AlgNotAllowed)));
    // A `crit` naming what isn't understood, and an unencoded payload.
    let crit = signer.sign(
        &json!({ "alg": "ES256", "kid": "key", "crit": ["exp"] }),
        &claims(json!({})),
    );
    assert_eq!(auth.check(&crit, at), Err(Some(JoseError::NotSupported)));
    let unencoded = signer.sign(
        &json!({ "alg": "ES256", "kid": "key", "crit": ["b64"], "b64": false }),
        &claims(json!({})),
    );
    assert_eq!(auth.authenticate(&unencoded), None);
    // A payload that isn't an object.
    let message = format!(
        "{}.{}",
        encode_url(header("key").to_string().as_bytes()),
        encode_url(b"[1]")
    );
    let signature = signer
        .key
        .sign(&SystemRandom::new(), message.as_bytes())
        .unwrap();
    let array = format!("{message}.{}", encode_url(signature.as_ref()));
    assert_eq!(auth.check(&array, at), Err(Some(JoseError::JwtInvalid)));
}

#[test]
fn duplicate_key_ids_and_invalid_keys_are_refused_before_the_service() {
    let signer = es256();
    let doubled = config(vec![
        key("key", Algorithm::Es256, &signer.public),
        key("key", Algorithm::Es256, &signer.public),
    ]);
    assert_eq!(
        BrowserAuthentication::new(doubled).unwrap_err(),
        "browser-auth: verification key ids must be unique"
    );
    let invalid = BrowserAuthentication::new(config(vec![key("key", Algorithm::Es256, "invalid")]))
        .unwrap_err();
    assert_eq!(
        invalid,
        "browser-auth: key \"key\": \"spki\" must be SPKI formatted string"
    );
    // A key of another type than its algorithm's.
    let mismatched =
        BrowserAuthentication::new(config(vec![key("key", Algorithm::Rs256, &signer.public)]))
            .unwrap_err();
    assert!(mismatched.contains("Invalid key type"), "{mismatched}");
    let short = include_str!("fixtures/rsa-1024.pub.pem");
    assert_eq!(
        BrowserAuthentication::new(config(vec![key("rsa", Algorithm::Rs256, short)])).unwrap_err(),
        "browser-auth: key \"rsa\": RS256 requires key modulusLength to be 2048 bits or larger"
    );
    let ed448 = include_str!("fixtures/ed448.pub.pem");
    assert!(BrowserAuthentication::new(config(vec![key("ed", Algorithm::EdDsa, ed448)])).is_err());
}

#[test]
fn rs256_and_ed25519_keys_verify_and_a_kid_must_match_its_algorithm() {
    let rsa = include_str!("fixtures/rsa-2048.pub.pem");
    let token = include_str!("fixtures/rs256.jwt").trim();
    let rng = SystemRandom::new();
    let ed =
        Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref()).unwrap();
    let mut der = ED25519_SPKI.to_vec();
    der.extend_from_slice(ed.public_key().as_ref());
    let es = es256();
    let auth = BrowserAuthentication::new(config(vec![
        key("rsa", Algorithm::Rs256, rsa),
        key("ed", Algorithm::EdDsa, &pem(&der)),
        key("es", Algorithm::Es256, &es.public),
    ]))
    .unwrap();
    // Signed with iat 1700000000; checked 10 s later.
    let caller = auth.authenticate_at(token, 1_700_000_010).unwrap();
    assert_eq!(caller.expires_at, 1_700_000_120_000);
    assert_eq!(auth.authenticate_at(token, 1_700_000_120), None);
    let sign_ed = |header: Value| {
        let message = format!(
            "{}.{}",
            encode_url(header.to_string().as_bytes()),
            encode_url(claims(json!({})).to_string().as_bytes())
        );
        format!(
            "{message}.{}",
            encode_url(ed.sign(message.as_bytes()).as_ref())
        )
    };
    assert!(
        auth.authenticate(&sign_ed(json!({ "alg": "EdDSA", "kid": "ed" })))
            .is_some()
    );
    // The right key, under another key's id.
    assert_eq!(
        auth.authenticate(&sign_ed(json!({ "alg": "EdDSA", "kid": "es" }))),
        None
    );
}

#[test]
fn the_row_config_reads_karakas_shape() {
    let config = BrowserAuthConfig::from_json(
        &json!({
            "applicationId": "app", "issuer": "i", "audience": "a", "maxTokenAgeSeconds": 300,
            "keys": [{ "id": "k", "algorithm": "ES256", "publicKey": "pem" }],
        }),
        "row",
    )
    .unwrap();
    assert_eq!(config.keys[0].algorithm, Algorithm::Es256);
    for (value, message) in [
        (
            json!({ "issuer": "i", "colour": 1 }),
            "row: unknown key \"colour\"",
        ),
        (
            json!({ "applicationId": "app", "issuer": "i", "audience": "a", "maxTokenAgeSeconds": 0, "keys": [] }),
            "row: \"maxTokenAgeSeconds\" must be a whole number from 1 to 2147483",
        ),
        (
            json!({ "applicationId": "app", "issuer": "i", "audience": "a", "maxTokenAgeSeconds": 5, "keys": [] }),
            "row: \"keys\" must hold at least one key",
        ),
        (
            json!({ "applicationId": "app", "issuer": "i", "audience": "a", "maxTokenAgeSeconds": 5, "keys": [{ "id": "k", "algorithm": "HS256", "publicKey": "p" }] }),
            "row: key 1: \"algorithm\" must be ES256, RS256 or EdDSA, not \"HS256\"",
        ),
    ] {
        assert_eq!(
            BrowserAuthConfig::from_json(&value, "row").unwrap_err(),
            message
        );
    }
}
