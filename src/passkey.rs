//! Sign-in with fingerprint or face recognition (WebAuthn, "passkey").
//!
//! The phone creates a key pair for this site; the private key never leaves the phone and only the public
//! key is stored here. To sign in, the phone signs the one-time challenge from the server after the
//! fingerprint / face / screen lock check. The key is bound to the domain: it cannot be used on another site
//! (e.g. a fake page).
//!
//! Only ES256 (P-256), the signature phones use, is supported. No attestation is requested; adding a key
//! requires the password instead.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::pkcs8::DecodePublicKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::i18n::Msg;

/// Time the phone has to sign the challenge.
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// Maximum challenges pending at once; limited because the sign-in screen is reachable without a session.
const MAX_PENDING: usize = 32;
/// User id given to the browser. Fixed, since there is a single user: adding the same phone again replaces
/// the old key on the phone.
pub const USER_ID: &[u8] = b"strcu-panel";

// Flags in the authenticator data
const UP: u8 = 0x01; // user present (touched)
const UV: u8 = 0x04; // fingerprint, face or screen lock verified
const AT: u8 = 0x40; // registration carries the new key

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Passkey {
    /// Key id (base64url)
    pub id: String,
    /// P-256 public key, SEC1 uncompressed point (base64url)
    pub key: String,
    /// Domain the key is bound to (e.g. strcu.example.com; localhost when testing on this computer)
    pub rp_id: String,
    /// Name shown in the panel, e.g. "Android · Chrome"
    pub name: String,
    pub created: String,
    #[serde(default)]
    pub last_used: Option<String>,
    /// The phone's signature counter. Synced keys (Google, iCloud) always report 0.
    #[serde(default)]
    pub sign_count: u32,
}

/// The site a key is valid for and the origin the browser must report.
#[derive(Clone, Debug, PartialEq)]
pub struct Site {
    pub rp_id: String,
    pub origin: String,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Register,
    Login,
}

struct Pending {
    challenge: [u8; 32],
    kind: Kind,
    rp_id: String,
    at: Instant,
}

/// Registration from the phone (result of navigator.credentials.create, base64url).
#[derive(Deserialize)]
pub struct Registration {
    pub id: String,
    pub client_data: String,
    pub auth_data: String,
    /// Public key, SubjectPublicKeyInfo (DER)
    pub public_key: String,
    pub alg: i64,
    #[serde(default)]
    pub name: String,
}

/// Sign-in signature from the phone (result of navigator.credentials.get, base64url).
#[derive(Deserialize)]
pub struct Assertion {
    pub id: String,
    pub client_data: String,
    pub auth_data: String,
    pub signature: String,
}

#[derive(Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    kind: String,
    challenge: String,
    origin: String,
}

pub struct Store {
    keys: Mutex<Vec<Passkey>>,
    pending: Mutex<Vec<Pending>>,
    save: fn(&[Passkey]) -> Result<()>,
}

impl Store {
    /// Saved keys are read from `config.json` and changes are written back there.
    pub fn load() -> Self {
        Self::new(crate::config::load().passkeys, |keys| crate::config::update(|c| c.passkeys = keys.to_vec()))
    }

    fn new(keys: Vec<Passkey>, save: fn(&[Passkey]) -> Result<()>) -> Self {
        Store { keys: Mutex::new(keys), pending: Mutex::default(), save }
    }

    pub fn list(&self) -> Vec<Passkey> {
        self.keys.lock().unwrap().clone()
    }

    /// Ids of the keys registered for this site.
    pub fn ids(&self, site: &Site) -> Vec<String> {
        self.keys.lock().unwrap().iter().filter(|k| k.rp_id == site.rp_id).map(|k| k.id.clone()).collect()
    }

    /// New one-time challenge (base64url).
    pub fn begin(&self, kind: Kind, site: &Site) -> String {
        let mut challenge = [0u8; 32];
        getrandom::fill(&mut challenge).expect("the operating system gave no random bytes");
        let mut p = self.pending.lock().unwrap();
        p.retain(|x| x.at.elapsed() < TIMEOUT);
        if p.len() >= MAX_PENDING {
            p.remove(0);
        }
        p.push(Pending { challenge, kind, rp_id: site.rp_id.clone(), at: Instant::now() });
        B64.encode(challenge)
    }

    /// Checks the client data the browser signed: the challenge must have been issued for this purpose and
    /// site (and is removed, so it cannot be used twice), and the type and origin must be right.
    fn client_data(&self, raw: &[u8], kind: Kind, site: &Site) -> Result<()> {
        let cd: ClientData = serde_json::from_slice(raw).with_context(|| invalid("client data"))?;
        let c = decode(&cd.challenge)?;
        let p = {
            let mut pending = self.pending.lock().unwrap();
            let i = pending.iter().position(|x| x.challenge[..] == c[..]).context(Msg::new("err.pk_challenge"))?;
            pending.remove(i)
        };
        ensure!(p.at.elapsed() < TIMEOUT, Msg::new("err.pk_timeout"));
        ensure!(p.kind == kind && p.rp_id == site.rp_id, Msg::new("err.pk_challenge"));
        let want = match kind {
            Kind::Register => "webauthn.create",
            Kind::Login => "webauthn.get",
        };
        ensure!(cd.kind == want, invalid(&format!("type {}", cd.kind)));
        ensure!(cd.origin == site.origin, Msg::new("err.pk_origin").with("origin", &cd.origin));
        Ok(())
    }

    /// Verifies and saves a new key. The caller checks the password.
    pub fn finish_register(&self, r: &Registration, site: &Site, now: &str) -> Result<Passkey> {
        self.client_data(&decode(&r.client_data)?, Kind::Register, site)?;
        ensure!(r.alg == -7, Msg::new("err.pk_alg"));
        let ad = decode(&r.auth_data)?;
        let count = check_auth_data(&ad, site, true)?;
        // The key id in the authenticator data must match the one the browser reported:
        // 37 byte header + 16 byte AAGUID + 2 byte length + id
        let raw_id = decode(&r.id)?;
        let n = ad.get(53..55).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).with_context(|| invalid("key data"))?;
        ensure!(!raw_id.is_empty() && ad.get(55..55 + n) == Some(&raw_id[..]), invalid("key id"));
        let key = p256::PublicKey::from_public_key_der(&decode(&r.public_key)?)
            .map_err(|_| Msg::new("err.pk_alg"))?;
        let pk = Passkey {
            id: B64.encode(&raw_id),
            key: B64.encode(key.to_encoded_point(false).as_bytes()),
            rp_id: site.rp_id.clone(),
            name: clean_name(&r.name),
            created: now.to_string(),
            last_used: None,
            sign_count: count,
        };
        let mut keys = self.keys.lock().unwrap();
        ensure!(!keys.iter().any(|k| k.id == pk.id), Msg::new("err.pk_exists"));
        let mut next = keys.clone();
        next.push(pk.clone());
        (self.save)(&next)?;
        *keys = next;
        Ok(pk)
    }

    /// Verifies a sign-in signature; if valid, returns the key's name.
    pub fn finish_login(&self, a: &Assertion, site: &Site, now: &str) -> Result<String> {
        let cd = decode(&a.client_data)?;
        self.client_data(&cd, Kind::Login, site)?;
        let id = B64.encode(decode(&a.id)?);
        let mut keys = self.keys.lock().unwrap();
        let i = keys.iter().position(|k| k.id == id && k.rp_id == site.rp_id).context(Msg::new("err.pk_unknown"))?;
        let ad = decode(&a.auth_data)?;
        let count = check_auth_data(&ad, site, false)?;
        let vk = VerifyingKey::from_sec1_bytes(&decode(&keys[i].key)?).map_err(|_| invalid("saved key"))?;
        let sig = Signature::from_der(&decode(&a.signature)?).map_err(|_| invalid("signature"))?;
        // Some devices sign in "high s" form; both are valid, normalised to one form for verification
        let sig = sig.normalize_s().unwrap_or(sig);
        let mut msg = ad.clone();
        msg.extend_from_slice(&Sha256::digest(&cd));
        vk.verify(&msg, &sig).map_err(|_| Msg::new("err.pk_signature"))?;
        // If the counter did not increase the key may have been cloned (synced keys always report 0)
        let old = keys[i].sign_count;
        ensure!((count == 0 && old == 0) || count > old, Msg::new("err.pk_counter").with("count", count).with("old", old));
        let mut next = keys.clone();
        next[i].sign_count = count;
        next[i].last_used = Some(now.to_string());
        // The sign-in is valid even if last use cannot be saved; it stays current in memory
        let _ = (self.save)(&next);
        *keys = next;
        Ok(keys[i].name.clone())
    }

    pub fn remove(&self, id: &str) -> Result<Passkey> {
        let mut keys = self.keys.lock().unwrap();
        let i = keys.iter().position(|k| k.id == id).context(Msg::new("err.pk_unknown"))?;
        let mut next = keys.clone();
        let gone = next.remove(i);
        (self.save)(&next)?;
        *keys = next;
        Ok(gone)
    }
}

/// Authenticator data: site hash, flags and signature counter.
fn check_auth_data(ad: &[u8], site: &Site, registering: bool) -> Result<u32> {
    ensure!(ad.len() >= 37, invalid("authenticator data"));
    ensure!(ad[..32] == Sha256::digest(site.rp_id.as_bytes())[..], Msg::new("err.pk_site"));
    let flags = ad[32];
    ensure!(flags & UP != 0, Msg::new("err.pk_presence"));
    ensure!(flags & UV != 0, Msg::new("err.pk_uv"));
    ensure!(!registering || flags & AT != 0, invalid("new key"));
    Ok(u32::from_be_bytes([ad[33], ad[34], ad[35], ad[36]]))
}

fn decode(s: &str) -> Result<Vec<u8>> {
    B64.decode(s.trim_end_matches('=')).with_context(|| invalid("base64url"))
}

/// Malformed data from the browser; `what` names the part (technical, not translated).
fn invalid(what: &str) -> Msg {
    Msg::new("err.pk_invalid").with("what", what)
}

fn clean_name(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).take(40).collect();
    match t.trim() {
        "" => "Phone".into(),
        t => t.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::SigningKey;
    use p256::ecdsa::signature::Signer;

    // P-256 SubjectPublicKeyInfo header; the 65 byte point follows
    const SPKI_PREFIX: [u8; 26] = [
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce,
        0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];

    fn site() -> Site {
        Site { rp_id: "strcu.example.com".into(), origin: "https://strcu.example.com".into() }
    }

    fn store() -> Store {
        Store::new(Vec::new(), |_| Ok(()))
    }

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32].into()).unwrap()
    }

    fn auth_data(rp: &str, flags: u8, count: u32, cred: Option<&[u8]>) -> Vec<u8> {
        let mut ad = Sha256::digest(rp.as_bytes()).to_vec();
        ad.push(flags);
        ad.extend_from_slice(&count.to_be_bytes());
        if let Some(id) = cred {
            ad.extend_from_slice(&[0u8; 16]);
            ad.extend_from_slice(&(id.len() as u16).to_be_bytes());
            ad.extend_from_slice(id);
            ad.extend_from_slice(&[0xa0]); // the COSE key is not read here
        }
        ad
    }

    fn client_data(kind: &str, challenge: &str, origin: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "type": kind, "challenge": challenge, "origin": origin })).unwrap()
    }

    fn register(s: &Store, cred: &[u8]) -> Result<Passkey> {
        let ch = s.begin(Kind::Register, &site());
        let mut spki = SPKI_PREFIX.to_vec();
        spki.extend_from_slice(signing_key().verifying_key().to_encoded_point(false).as_bytes());
        let r = Registration {
            id: B64.encode(cred),
            client_data: B64.encode(client_data("webauthn.create", &ch, "https://strcu.example.com")),
            auth_data: B64.encode(auth_data("strcu.example.com", UP | UV | AT, 0, Some(cred))),
            public_key: B64.encode(spki),
            alg: -7,
            name: "Android · Chrome".into(),
        };
        s.finish_register(&r, &site(), "2026-10-04 15:00:00")
    }

    fn assertion(s: &Store, cred: &[u8], origin: &str, flags: u8, count: u32) -> Assertion {
        let ch = s.begin(Kind::Login, &site());
        let cd = client_data("webauthn.get", &ch, origin);
        let ad = auth_data("strcu.example.com", flags, count, None);
        let mut msg = ad.clone();
        msg.extend_from_slice(&Sha256::digest(&cd));
        let sig: Signature = signing_key().sign(&msg);
        Assertion {
            id: B64.encode(cred),
            client_data: B64.encode(cd),
            auth_data: B64.encode(ad),
            signature: B64.encode(sig.to_der()),
        }
    }

    #[test]
    fn register_and_sign_in() {
        let s = store();
        let pk = register(&s, b"phone-1").unwrap();
        assert_eq!(pk.rp_id, "strcu.example.com");
        assert_eq!(s.ids(&site()), vec![B64.encode(b"phone-1")]);
        let a = assertion(&s, b"phone-1", "https://strcu.example.com", UP | UV, 0);
        assert_eq!(s.finish_login(&a, &site(), "2026-10-04 15:01:00").unwrap(), "Android · Chrome");
        assert_eq!(s.list()[0].last_used.as_deref(), Some("2026-10-04 15:01:00"));
        // The same phone cannot be added twice
        assert!(register(&s, b"phone-1").is_err());
    }

    #[test]
    fn challenge_cannot_be_reused() {
        let s = store();
        register(&s, b"t").unwrap();
        let a = assertion(&s, b"t", "https://strcu.example.com", UP | UV, 0);
        s.finish_login(&a, &site(), "x").unwrap();
        assert!(s.finish_login(&a, &site(), "x").is_err());
    }

    #[test]
    fn forged_signature_and_wrong_origin_are_rejected() {
        let s = store();
        register(&s, b"t").unwrap();
        // One bit of the signature flipped
        let mut a = assertion(&s, b"t", "https://strcu.example.com", UP | UV, 0);
        let mut sig = decode(&a.signature).unwrap();
        let last = sig.len() - 1;
        sig[last] ^= 1;
        a.signature = B64.encode(sig);
        assert!(s.finish_login(&a, &site(), "x").is_err());
        // Another origin (fake page)
        let a = assertion(&s, b"t", "https://fake.example.com", UP | UV, 0);
        assert!(s.finish_login(&a, &site(), "x").is_err());
        // Fingerprint not verified (touch only)
        let a = assertion(&s, b"t", "https://strcu.example.com", UP, 0);
        assert!(s.finish_login(&a, &site(), "x").is_err());
        // Unregistered key
        let a = assertion(&s, b"other", "https://strcu.example.com", UP | UV, 0);
        assert!(s.finish_login(&a, &site(), "x").is_err());
    }

    #[test]
    fn login_challenge_cannot_register() {
        let s = store();
        let ch = s.begin(Kind::Login, &site());
        let cd = client_data("webauthn.create", &ch, "https://strcu.example.com");
        assert!(s.client_data(&cd, Kind::Register, &site()).is_err());
        // Challenge issued for another site
        let other = Site { rp_id: "localhost".into(), origin: "http://localhost:8765".into() };
        let ch = s.begin(Kind::Login, &other);
        let cd = client_data("webauthn.get", &ch, "https://strcu.example.com");
        assert!(s.client_data(&cd, Kind::Login, &site()).is_err());
    }

    #[test]
    fn counter_cannot_go_back() {
        let s = store();
        register(&s, b"t").unwrap();
        let a = assertion(&s, b"t", "https://strcu.example.com", UP | UV, 5);
        s.finish_login(&a, &site(), "x").unwrap();
        let a = assertion(&s, b"t", "https://strcu.example.com", UP | UV, 5);
        assert!(s.finish_login(&a, &site(), "x").is_err());
        let a = assertion(&s, b"t", "https://strcu.example.com", UP | UV, 6);
        assert!(s.finish_login(&a, &site(), "x").is_ok());
    }

    #[test]
    fn removal() {
        let s = store();
        let pk = register(&s, b"t").unwrap();
        assert_eq!(s.remove(&pk.id).unwrap().name, "Android · Chrome");
        assert!(s.ids(&site()).is_empty());
        assert!(s.remove(&pk.id).is_err());
    }
}
