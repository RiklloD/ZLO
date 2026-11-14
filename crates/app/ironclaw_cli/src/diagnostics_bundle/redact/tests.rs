use super::*;

const TOK: &str = "q7Zp2mWx9LrT4vNc";

fn scrubber() -> Scrubber {
    Scrubber::new(["known-secret-value-123".to_string(), "short".to_string()])
}

fn assert_clean(input: &str, needles: &[&str]) -> String {
    let out = scrubber().text(input);
    for needle in needles {
        assert!(
            !out.contains(needle),
            "{input:?} -> {out:?} leaks {needle:?}"
        );
    }
    out
}

#[test]
fn url_userinfo_query_and_fragment_are_removed() {
    let out =
        scrubber().text("connect postgres://admin:hunter2pw@db.internal:5432/zlo?sslkey=abc#x");
    assert!(
        out.starts_with("connect postgres://[REDACTED]@db.internal:5432/zlo?"),
        "{out}"
    );
    assert!(!out.contains("hunter2pw") && !out.contains("abc"), "{out}");
}

#[test]
fn credential_pairs_in_every_spacing_and_quoting_are_removed() {
    for input in [
        format!("authorization=Bearer {TOK}"),
        format!("authorization=\"Bearer {TOK}\""),
        format!("Authorization:Bearer {TOK}"),
        format!("Authorization:\tBearer\t{TOK}"),
        format!("Authorization: Bearer {TOK} trailing"),
        format!("sent Bearer\t{TOK}"),
        format!("basic {TOK}"),
        format!("--token {TOK}"),
        format!("--api-key {TOK}"),
        format!("--api-key=\"{TOK}\""),
        format!("api_key = {TOK}"),
        format!("{{\"token\" : \"{TOK}\"}} x"),
        format!("payload={{\\\"token\\\":\\\"{TOK}\\\"}}"),
        format!("pass={TOK} pwd={TOK}"),
        format!("auth=user:{TOK}"),
        format!("token => {TOK}"),
    ] {
        assert_clean(&input, &[TOK]);
    }
    let out = assert_clean("password=\"two word password\" next", &["two", "word p"]);
    assert!(out.starts_with("password=\"[REDACTED]\""), "{out}");
    assert!(out.ends_with(" next"), "{out}");
    assert_clean("api_key=\"two words\"", &["two", "words"]);
}

#[test]
fn env_var_names_and_token_counts_survive() {
    let out = scrubber().text("api_key_env=OPENAI_API_KEY input_tokens=42 model=gpt-5-mini");
    assert_eq!(
        out,
        "api_key_env=OPENAI_API_KEY input_tokens=42 model=gpt-5-mini"
    );
    assert_eq!(
        scrubber().text("bypass author=ana pinned=3"),
        "bypass author=ana pinned=3"
    );
}

#[test]
fn store_only_secret_shapes_are_removed() {
    let telegram = format!("{}:{}", "1234567890", "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw");
    let notion = format!("{}{}", "secret_", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4");
    let ntn = format!("{}{}", "ntn_", "4242424242aBcDeFgHiJkLmNoPqRs");
    let refresh = format!("{}{}", "1//", "0gAbCdEfGhIjKlMnOpQrStUvWxYz-_12");
    assert_clean(&format!("bot token {telegram} ok"), &[&telegram, "AAHdq"]);
    assert_clean(
        &format!("GET https://api.telegram.org/bot{telegram}/getMe"),
        &["AAHdq"],
    );
    assert_clean(&format!("notion {notion} and {ntn}"), &[&notion, &ntn]);
    assert_clean(&format!("refresh {refresh}"), &["0gAbCdEf"]);
    assert_clean(&format!("x:{refresh}"), &["0gAbCdEf"]);
}

#[test]
fn pem_blocks_are_removed_whole() {
    let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC";
    let pem = format!("-----BEGIN PRIVATE KEY-----\n{body}\nabc\n-----END PRIVATE KEY-----");
    let out = assert_clean(&format!("before\n{pem}\nafter"), &[body, "abc"]);
    assert_eq!(out, "before\n[REDACTED]\nafter");
    assert_clean(
        &format!("{{\"k\":\"{}\"}}", pem.replace('\n', "\\n")),
        &[body],
    );
    assert_clean(&format!("-----BEGIN RSA PRIVATE KEY-----\n{body}"), &[body]);
    assert_clean(
        &format!("{body}\n-----END RSA PRIVATE KEY-----\nok"),
        &[body],
    );
}

#[test]
fn known_values_are_removed_in_every_enumerated_spelling() {
    let secret = "kn0wn/S3cret+va\"lue";
    let scrub = Scrubber::new([secret.to_string()]);
    let escaped = serde_json::to_string(secret).expect("json");
    let mut spellings = vec![
        secret.to_string(),
        escaped[1..escaped.len() - 1].to_string(),
        "kn0wn%2FS3cret%2Bva%22lue".to_string(),
        "kn0wn%2fS3cret%2bva%22lue".to_string(),
    ];
    spellings.push(
        secret
            .chars()
            .map(|c| format!("\\u{:04x}", c as u32))
            .collect(),
    );
    for form in encodings::forms(secret) {
        spellings.push(form);
    }
    for spelling in spellings {
        let out = scrub.text(&format!("innocuous_field={spelling} end"));
        assert!(
            !scrub.contains_known_secret(out.as_bytes()),
            "{spelling} -> {out}"
        );
    }
    // A spelling replacement does not enumerate is still caught at the end.
    assert!(scrub.contains_known_secret(b"x=kn0wn%2FS3cret+va\\u0022lue"));
}

#[test]
fn ansi_escapes_are_stripped() {
    assert_eq!(
        scrubber().text("\u{1b}[2m2026\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m ok"),
        "2026  INFO ok"
    );
}

#[test]
fn json_replaces_any_value_under_sensitive_keys_and_scrubs_keys() {
    let mut value = serde_json::json!({
        "webui.env_token_var": "ZLO_WEBUI_TOKEN",
        "client_secret": "abc",
        "token": {"value": "nested-token-value"},
        "api_key": ["array-key-value"],
        "pin": 4321,
        "summary": {"pass": 5, "fail": 0},
        "has_token": true,
        "known-secret-value-123": "x",
        "nested": [{"detail": "see https://u:p@h/x?y=z"}],
        "count": 3
    });
    scrubber().json(&mut value);
    let text = value.to_string();
    for leak in [
        "abc",
        "nested-token-value",
        "array-key-value",
        "4321",
        "known-secret-value-123",
    ] {
        assert!(!text.contains(leak), "{text}");
    }
    assert_eq!(value["webui.env_token_var"], "ZLO_WEBUI_TOKEN");
    assert_eq!(value["token"], REDACTED);
    assert_eq!(value["summary"]["pass"], 5);
    assert_eq!(value["has_token"], true);
    assert_eq!(value["count"], 3);
    assert_eq!(
        value["nested"][0]["detail"],
        "see https://[REDACTED]@h/x?[REDACTED]"
    );
}

#[test]
fn json_inside_a_log_line_is_scrubbed_structurally() {
    let out = assert_clean(
        "INFO body={\"auth\":{\"user\":\"ana\",\"password\":\"p w\"},\"pin\":1234} done",
        &["p w", "1234"],
    );
    assert!(
        out.contains("\"user\":\"ana\"") && out.ends_with(" done"),
        "{out}"
    );
    assert_clean("token={\"v\":\"x9x9x9x9\"}", &["x9x9x9x9"]);
}

/// Known secrets of 4–7 characters are replaced, and detected by the final
/// check, only as whole tokens — in every enumerated spelling and after the
/// final check's lenient decoding; under 4 they are flagged unredactable.
#[test]
fn short_known_secrets_match_as_whole_tokens_only() {
    let scrubber = Scrubber::new(["Xy7pQ2w".to_string()]);
    assert!(!scrubber.has_unredactable_secret());
    assert_eq!(
        scrubber.text("auth Xy7pQ2w, notXy7pQ2wpart"),
        "auth [REDACTED], notXy7pQ2wpart"
    );
    assert!(scrubber.contains_known_secret(b"left Xy7pQ2w right"));
    // A mixed spelling replacement does not enumerate still trips the check.
    assert!(scrubber.contains_known_secret(b"left Xy%37pQ2w right"));
    assert!(!scrubber.contains_known_secret(b"inside aXy7pQ2wb a word"));

    assert!(Scrubber::new(["a1b".to_string()]).has_unredactable_secret());
    assert!(!Scrubber::new([String::new()]).has_unredactable_secret());
}

/// A short known secret inside a larger base64 / base64url blob has no
/// spelling of its own: the blob is decoded, the token rule applied to the
/// decoded text, and the whole blob replaced — and the final check finds it
/// the same way.
#[test]
fn short_secret_inside_a_base64_blob_is_found_by_decoding() {
    let scrubber = Scrubber::new(["Xy7pQ2w".to_string()]);
    // base64("user:Xy7pQ2w:suffix") and base64url-unpadded of the same.
    let padded = "dXNlcjpYeTdwUTJ3OnN1ZmZpeA==";
    let unpadded = "dXNlcjpYeTdwUTJ3OnN1ZmZpeA";
    for blob in [padded, unpadded] {
        assert!(
            scrubber.contains_known_secret(format!("data {blob} end").as_bytes()),
            "{blob}"
        );
        assert_eq!(
            scrubber.text(&format!("data {blob} end")),
            "data [REDACTED] end"
        );
    }
    // base64("aXy7pQ2wb"): the secret is inside a longer word, not a token.
    let inside_word = "YVh5N3BRMndi";
    assert!(!scrubber.contains_known_secret(format!("data {inside_word}").as_bytes()));
    assert_eq!(
        scrubber.text(&format!("data {inside_word}")),
        format!("data {inside_word}")
    );
}
