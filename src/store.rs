// Product registry + purchase ledger, persisted as JSON under the products dir.
//
// Products are uploaded by the artist (NIP-98 authenticated) and sold through
// the L402-style gate in gate.rs. A purchase is created when an invoice is
// issued and marked paid either by the LDK event loop (PaymentReceived) or
// lazily at download time by checking the node's payment store — so paid
// purchases survive daemon restarts and event-loop races.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProductRecord {
    pub slug: String,
    pub title: String,
    pub price_sats: u64,
    /// Some(_) = name-your-price with this minimum.
    pub floor_sats: Option<u64>,
    pub format: String,
    /// File name inside the products dir (never a path — see artifact_path).
    pub file_name: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PurchaseRecord {
    /// Hex payment hash of the invoice issued for this purchase.
    pub payment_hash: String,
    pub slug: String,
    pub amount_msat: u64,
    pub paid: bool,
    pub created_at: u64,
    /// Session secret returned only to the invoice requester — lets a web
    /// buyer (whose wallet holds the preimage) poll status and claim the
    /// download. Absent on records from before this field existed.
    #[serde(default)]
    pub claim_token: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    products: HashMap<String, ProductRecord>,
    purchases: HashMap<String, PurchaseRecord>,
}

pub struct Store {
    dir: PathBuf,
    state: Persisted,
}

impl Store {
    /// Load (or initialize) the store rooted at `<data_dir>/products`.
    pub fn open(data_dir: &str) -> Result<Self, String> {
        let dir = Path::new(data_dir).join("products");
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create products dir: {e}"))?;

        let index = dir.join("index.json");
        let state = if index.exists() {
            let raw = std::fs::read_to_string(&index)
                .map_err(|e| format!("Failed to read product index: {e}"))?;
            serde_json::from_str(&raw)
                .map_err(|e| format!("Corrupt product index: {e}"))?
        } else {
            Persisted::default()
        };

        Ok(Store { dir, state })
    }

    fn persist(&self) -> Result<(), String> {
        let raw = serde_json::to_string_pretty(&self.state)
            .map_err(|e| format!("Failed to serialize product index: {e}"))?;
        let tmp = self.dir.join("index.json.tmp");
        std::fs::write(&tmp, raw).map_err(|e| format!("Failed to write index: {e}"))?;
        std::fs::rename(&tmp, self.dir.join("index.json"))
            .map_err(|e| format!("Failed to commit index: {e}"))?;
        Ok(())
    }

    /// Absolute path of a product's artifact file.
    pub fn artifact_path(&self, product: &ProductRecord) -> PathBuf {
        self.dir.join(&product.file_name)
    }

    pub fn products_dir(&self) -> &Path {
        &self.dir
    }

    pub fn get_product(&self, slug: &str) -> Option<&ProductRecord> {
        self.state.products.get(slug)
    }

    pub fn upsert_product(&mut self, record: ProductRecord) -> Result<(), String> {
        self.state.products.insert(record.slug.clone(), record);
        self.persist()
    }

    pub fn create_purchase(
        &mut self,
        payment_hash: &str,
        slug: &str,
        amount_msat: u64,
        created_at: u64,
        claim_token: &str,
    ) -> Result<(), String> {
        self.state.purchases.insert(
            payment_hash.to_string(),
            PurchaseRecord {
                payment_hash: payment_hash.to_string(),
                slug: slug.to_string(),
                amount_msat,
                paid: false,
                created_at,
                claim_token: Some(claim_token.to_string()),
            },
        );
        self.persist()
    }

    pub fn get_purchase(&self, payment_hash: &str) -> Option<&PurchaseRecord> {
        self.state.purchases.get(payment_hash)
    }

    /// Look up a purchase by its claim token (web-buyer path).
    pub fn get_purchase_by_claim(&self, claim_token: &str) -> Option<&PurchaseRecord> {
        self.state
            .purchases
            .values()
            .find(|p| p.claim_token.as_deref() == Some(claim_token))
    }

    /// Mark a purchase paid. Unknown hashes are fine (e.g. streaming keysends
    /// hitting the same node) — returns whether a purchase matched.
    pub fn mark_paid(&mut self, payment_hash: &str) -> Result<bool, String> {
        match self.state.purchases.get_mut(payment_hash) {
            Some(p) if !p.paid => {
                p.paid = true;
                self.persist()?;
                Ok(true)
            }
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }
}

/// Validate a product slug for use as a d-tag identifier and file-name stem.
/// Rejects anything that could traverse paths.
pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 128
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Validate an artifact format extension ("flac", "wav", ...). Kept in
/// lockstep with the hosted gate's FORMATS set (marketing site,
/// api/gate/products/[slug]/route.ts) — the two sellers must not disagree
/// about what is sellable. "zip" covers stems bundles.
pub fn valid_format(format: &str) -> bool {
    matches!(
        format,
        "flac" | "wav" | "aiff" | "alac" | "mp3" | "ogg" | "m4a" | "aac" | "opus" | "zip"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().to_str().unwrap()).expect("open");
        (store, dir)
    }

    fn product(slug: &str) -> ProductRecord {
        ProductRecord {
            slug: slug.into(),
            title: "Midnight".into(),
            price_sats: 5000,
            floor_sats: None,
            format: "flac".into(),
            file_name: format!("{slug}.flac"),
            size_bytes: 1234,
        }
    }

    #[test]
    fn products_round_trip_through_persistence() {
        let (mut store, dir) = tmp_store();
        store.upsert_product(product("midnight")).unwrap();

        let reopened = Store::open(dir.path().to_str().unwrap()).unwrap();
        let p = reopened.get_product("midnight").expect("persisted");
        assert_eq!(p.price_sats, 5000);
        assert_eq!(p.file_name, "midnight.flac");
    }

    #[test]
    fn purchase_lifecycle_persists() {
        let (mut store, dir) = tmp_store();
        store.upsert_product(product("midnight")).unwrap();
        store
            .create_purchase("aa".repeat(32).as_str(), "midnight", 5_000_000, 1000, "tok-1")
            .unwrap();

        assert!(!store.get_purchase(&"aa".repeat(32)).unwrap().paid);
        assert!(store.mark_paid(&"aa".repeat(32)).unwrap());

        let reopened = Store::open(dir.path().to_str().unwrap()).unwrap();
        assert!(reopened.get_purchase(&"aa".repeat(32)).unwrap().paid);
    }

    #[test]
    fn mark_paid_unknown_hash_is_no_match() {
        let (mut store, _dir) = tmp_store();
        assert!(!store.mark_paid("ff00").unwrap());
    }

    #[test]
    fn purchase_found_by_claim_token() {
        let (mut store, _dir) = tmp_store();
        store
            .create_purchase("bb".repeat(32).as_str(), "midnight", 5_000_000, 1000, "tok-xyz")
            .unwrap();
        assert_eq!(
            store.get_purchase_by_claim("tok-xyz").unwrap().slug,
            "midnight"
        );
        assert!(store.get_purchase_by_claim("tok-nope").is_none());
    }

    #[test]
    fn slug_validation() {
        assert!(valid_slug("midnight-flac-24"));
        assert!(!valid_slug(""));
        assert!(!valid_slug("../etc/passwd"));
        assert!(!valid_slug("Has Spaces"));
        assert!(!valid_slug("UPPER"));
    }

    #[test]
    fn format_validation() {
        assert!(valid_format("flac"));
        assert!(valid_format("zip"));
        assert!(!valid_format("exe"));
        assert!(!valid_format(""));
    }
}
