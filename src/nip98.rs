// NIP-98 HTTP auth verification.
//
// The desktop app authenticates artifact uploads with a signed kind-27235
// event carried in `Authorization: Nostr <base64(event-json)>`. We verify:
// signature, kind, method + URL tags, payload hash (for bodies), freshness,
// and that the signer is the configured artist.

use base64::Engine;
use nostr::{Event, JsonUtil, Kind, PublicKey, Tag};

const AUTH_WINDOW_SECS: u64 = 60;

/// Verify a NIP-98 Authorization header. Returns the verified signer pubkey.
///
/// `expected_url` must exactly match the event's `u` tag; `body_sha256_hex`
/// is required for methods that carry a body (PUT/POST) and must match the
/// event's `payload` tag.
pub fn verify(
    auth_header: &str,
    method: &str,
    expected_url: &str,
    body_sha256_hex: Option<&str>,
    expected_pubkey: &PublicKey,
    now_secs: u64,
) -> Result<PublicKey, String> {
    let b64 = auth_header
        .strip_prefix("Nostr ")
        .ok_or("Authorization header must be 'Nostr <base64-event>'")?;

    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| "Invalid base64 in Authorization header")?;
    let json = String::from_utf8(raw).map_err(|_| "Authorization event is not UTF-8")?;

    let event = Event::from_json(&json).map_err(|e| format!("Invalid auth event: {e}"))?;
    event
        .verify()
        .map_err(|_| "Auth event signature verification failed")?;

    if event.kind != Kind::HttpAuth {
        return Err(format!("Auth event kind must be 27235, got {}", event.kind));
    }

    let tag_value = |name: &str| -> Option<String> {
        event.tags.iter().find_map(|t: &Tag| {
            let v = t.as_slice();
            (v.len() >= 2 && v[0] == name).then(|| v[1].to_string())
        })
    };

    let url = tag_value("u").ok_or("Auth event missing u tag")?;
    if url != expected_url {
        return Err(format!("Auth URL mismatch: {url} != {expected_url}"));
    }

    let m = tag_value("method").ok_or("Auth event missing method tag")?;
    if !m.eq_ignore_ascii_case(method) {
        return Err(format!("Auth method mismatch: {m} != {method}"));
    }

    let created = event.created_at.as_u64();
    if created + AUTH_WINDOW_SECS < now_secs || created > now_secs + AUTH_WINDOW_SECS {
        return Err("Auth event outside freshness window".into());
    }

    if let Some(expected_hash) = body_sha256_hex {
        let payload = tag_value("payload").ok_or("Auth event missing payload tag")?;
        if !payload.eq_ignore_ascii_case(expected_hash) {
            return Err("Auth payload hash does not match request body".into());
        }
    }

    if event.pubkey != *expected_pubkey {
        return Err("Auth event signed by a different key than the configured artist".into());
    }

    Ok(event.pubkey)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use nostr::{EventBuilder, Keys, Tag, TagKind};

    const URL: &str = "https://node.example.com/products/midnight";

    fn auth_header(keys: &Keys, url: &str, method: &str, payload: Option<&str>, at: u64) -> String {
        let mut tags = vec![
            Tag::custom(TagKind::custom("u"), vec![url.to_string()]),
            Tag::custom(TagKind::custom("method"), vec![method.to_string()]),
        ];
        if let Some(p) = payload {
            tags.push(Tag::custom(TagKind::custom("payload"), vec![p.to_string()]));
        }
        let event = EventBuilder::new(Kind::HttpAuth, "")
            .tags(tags)
            .custom_created_at(at.into())
            .sign_with_keys(keys)
            .expect("sign");
        format!(
            "Nostr {}",
            base64::engine::general_purpose::STANDARD.encode(event.as_json())
        )
    }

    #[test]
    fn valid_auth_passes() {
        let keys = Keys::generate();
        let header = auth_header(&keys, URL, "PUT", Some("ab12"), 1000);
        let pk = verify(&header, "PUT", URL, Some("ab12"), &keys.public_key(), 1010).unwrap();
        assert_eq!(pk, keys.public_key());
    }

    #[test]
    fn wrong_signer_rejected() {
        let keys = Keys::generate();
        let other = Keys::generate();
        let header = auth_header(&keys, URL, "PUT", None, 1000);
        assert!(verify(&header, "PUT", URL, None, &other.public_key(), 1010).is_err());
    }

    #[test]
    fn url_mismatch_rejected() {
        let keys = Keys::generate();
        let header = auth_header(&keys, "https://evil.example.com/x", "PUT", None, 1000);
        assert!(verify(&header, "PUT", URL, None, &keys.public_key(), 1010).is_err());
    }

    #[test]
    fn stale_auth_rejected() {
        let keys = Keys::generate();
        let header = auth_header(&keys, URL, "PUT", None, 1000);
        assert!(verify(&header, "PUT", URL, None, &keys.public_key(), 5000).is_err());
    }

    #[test]
    fn payload_mismatch_rejected() {
        let keys = Keys::generate();
        let header = auth_header(&keys, URL, "PUT", Some("aaaa"), 1000);
        assert!(verify(&header, "PUT", URL, Some("bbbb"), &keys.public_key(), 1010).is_err());
    }

    #[test]
    fn tampered_event_rejected() {
        let keys = Keys::generate();
        let header = auth_header(&keys, URL, "PUT", None, 1000);
        // Decode, tamper with the pubkey, re-encode
        let raw = base64::engine::general_purpose::STANDARD
            .decode(header.strip_prefix("Nostr ").unwrap())
            .unwrap();
        let tampered = String::from_utf8(raw)
            .unwrap()
            .replace(&keys.public_key().to_hex()[..8], "deadbeef");
        let tampered_header = format!(
            "Nostr {}",
            base64::engine::general_purpose::STANDARD.encode(tampered)
        );
        assert!(verify(&tampered_header, "PUT", URL, None, &keys.public_key(), 1010).is_err());
    }
}
