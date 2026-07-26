// Dev helper — signs NIP-98 auth headers for exercising the product gate
// from scripts (see infra/dev-cluster/scripts/purchase-test.sh).
//
// Usage:
//   nip98_sign pubkey <secret_hex>
//   nip98_sign header <secret_hex> <method> <url> [payload_sha256_hex]
//
// Prints the hex pubkey, or the full "Nostr <base64>" Authorization value.
// Regtest tooling only — never point this at real keys.

use base64::Engine;
use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag, TagKind};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: nip98_sign pubkey <secret_hex> | nip98_sign header <secret_hex> <method> <url> [payload_sha256]";

    match args.get(1).map(String::as_str) {
        Some("pubkey") => {
            let secret = args.get(2).expect(usage);
            let keys = Keys::parse(secret).expect("invalid secret key hex");
            println!("{}", keys.public_key().to_hex());
        }
        Some("header") => {
            let secret = args.get(2).expect(usage);
            let method = args.get(3).expect(usage);
            let url = args.get(4).expect(usage);
            let keys = Keys::parse(secret).expect("invalid secret key hex");

            let mut tags = vec![
                Tag::custom(TagKind::custom("u"), vec![url.to_string()]),
                Tag::custom(TagKind::custom("method"), vec![method.to_string()]),
            ];
            if let Some(payload) = args.get(5) {
                tags.push(Tag::custom(
                    TagKind::custom("payload"),
                    vec![payload.to_string()],
                ));
            }

            let event = EventBuilder::new(Kind::HttpAuth, "")
                .tags(tags)
                .sign_with_keys(&keys)
                .expect("failed to sign auth event");

            println!(
                "Nostr {}",
                base64::engine::general_purpose::STANDARD.encode(event.as_json())
            );
        }
        _ => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}
