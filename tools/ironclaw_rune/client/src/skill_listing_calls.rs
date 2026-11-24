//! How each skill is listed to the models, and its pin. Read leniently: the
//! Skills page shows this beside the skills themselves, so a listing from a
//! newer or older backend (a state this client does not know, a field added
//! or gone) loses only what it cannot read, never the page.

use ironclaw_product_contracts::skill_listing::{
    RebornSkillListingEntry, RebornSkillListingResponse, RebornSkillListingState,
};
use reqwest::Method;
use serde_json::{Value, json};

use crate::api::{Client, V2, seg};
use crate::error::ClientError;

impl Client {
    /// How each skill is listed to the models (full, name only, off), by
    /// which source, with its uses and pin.
    pub async fn skill_listing(&self) -> Result<RebornSkillListingResponse, ClientError> {
        let raw: Value = self.get(&format!("{V2}/skill-listing")).await?;
        Ok(lenient_listing(&raw))
    }

    /// Pin how one skill is listed (`None` back to automatic); answers with
    /// the listing as stored.
    pub async fn set_skill_listing_pin(
        &self,
        name: &str,
        pin: Option<RebornSkillListingState>,
    ) -> Result<RebornSkillListingResponse, ClientError> {
        let raw: Value = self
            .send(
                self.request(
                    Method::PUT,
                    &format!("{V2}/skill-listing/{}/pin", seg(name)),
                )
                .json(&json!({ "pin": pin })),
            )
            .await?;
        Ok(lenient_listing(&raw))
    }
}

/// The listing as far as this client reads it: unknown fields are ignored,
/// missing counts read as none, and a skill it cannot read (no name, or a
/// listing or pin state it does not know) is left out on its own. Without a
/// readable `managed` the backend is taken not to manage listings, which
/// the page shows as no listing at all.
fn lenient_listing(raw: &Value) -> RebornSkillListingResponse {
    let skills = raw
        .get("skills")
        .and_then(Value::as_array)
        .map(|skills| skills.iter().filter_map(lenient_entry).collect())
        .unwrap_or_default();
    RebornSkillListingResponse {
        managed: raw.get("managed").and_then(Value::as_bool).unwrap_or(false),
        skills,
    }
}

fn lenient_entry(raw: &Value) -> Option<RebornSkillListingEntry> {
    let mut entry = raw.as_object()?.clone();
    entry.entry("uses").or_insert(json!(0));
    entry.entry("sources").or_insert(json!([]));
    match serde_json::from_value(Value::Object(entry)) {
        Ok(entry) => Some(entry),
        Err(error) => {
            tracing::debug!(%error, "skill listing: an entry this client cannot read is left out");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{Connection, Token};
    use crate::test_http::serve;

    fn connect(url: String) -> Client {
        Client::new(Connection {
            base_url: url,
            token: Token::new("icd_dev_tok").unwrap(),
            token_source: "test".into(),
        })
        .unwrap()
    }

    /// A listing with fields this client does not know, counts it lacks, a
    /// state from a newer backend and an entry with no name: the readable
    /// skills come through, the others are left out, nothing fails.
    const MIXED: &str = r#"{"managed":true,"budget":{"tokens":900},"skills":[
        {"name":"pdf","uses":3,"last_used_ms":7,"listing":"full","pin":null,
         "sources":["harness","cli:codex"],"basis":"auto","score":0.4},
        {"name":"slim","listing":"name_only","pin":"always_full"},
        {"name":"future","uses":1,"listing":"summarized","sources":[]},
        {"name":"future-pin","uses":1,"listing":"off","pin":"sometimes","sources":[]},
        {"uses":2,"listing":"off","sources":[]},
        "not an entry"
    ]}"#;

    #[tokio::test]
    async fn the_listing_reads_what_it_can_and_leaves_the_rest_out() {
        assert!(
            serde_json::from_str::<RebornSkillListingResponse>(MIXED).is_err(),
            "read strictly, one unknown state fails the whole listing"
        );
        let (base, seen) = serve(200, MIXED).await;
        let client = connect(base);
        for listing in [
            client.skill_listing().await.unwrap(),
            client
                .set_skill_listing_pin("pdf", Some(RebornSkillListingState::Off))
                .await
                .unwrap(),
        ] {
            assert!(listing.managed);
            let names: Vec<&str> = listing.skills.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["pdf", "slim"], "{listing:?}");
            let pdf = &listing.skills[0];
            assert_eq!((pdf.uses, pdf.last_used_ms), (3, Some(7)));
            assert_eq!(pdf.sources, ["harness", "cli:codex"]);
            let slim = &listing.skills[1];
            assert_eq!((slim.uses, slim.last_used_ms), (0, None));
            assert!(slim.sources.is_empty());
            assert_eq!(slim.listing, RebornSkillListingState::NameOnly);
            assert_eq!(slim.pin, Some(RebornSkillListingState::Full));
        }
        let seen = seen.lock().unwrap();
        assert!(
            seen[0]
                .line
                .starts_with("GET /api/webchat/v2/skill-listing"),
            "{}",
            seen[0].line
        );
        assert!(
            seen[1].line.contains("/skill-listing/pdf/pin"),
            "{}",
            seen[1].line
        );
        assert_eq!(
            serde_json::from_str::<Value>(&seen[1].body).unwrap(),
            json!({"pin": "off"})
        );
    }

    /// A body with no listing in it reads as an unmanaged, empty listing.
    #[tokio::test]
    async fn a_listing_without_its_fields_reads_as_none() {
        let (base, _) = serve(200, r#"{"skills":"later"}"#).await;
        let listing = connect(base).skill_listing().await.unwrap();
        assert!(!listing.managed && listing.skills.is_empty(), "{listing:?}");
    }
}
