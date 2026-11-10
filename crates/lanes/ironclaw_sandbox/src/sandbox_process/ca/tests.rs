use super::*;
use secrecy::ExposeSecret;
use x509_parser::prelude::*;

fn parse<'a>(pem: &'a str) -> X509Certificate<'a> {
    let (_, parsed) = parse_x509_pem(pem.as_bytes()).expect("valid PEM");
    let cert = Box::leak(Box::new(parsed));
    cert.parse_x509().expect("valid X.509 DER")
}

#[test]
fn root_is_generated_once_and_reused() {
    let ca = SandboxCertificateAuthority::generate().unwrap();

    // Same in-memory root every call — not regenerated per access.
    assert_eq!(ca.root_certificate_pem(), ca.root_certificate_pem());
}

#[test]
fn root_certificate_has_ca_and_key_cert_sign_constraints() {
    let ca = SandboxCertificateAuthority::generate().unwrap();
    let root = parse(ca.root_certificate_pem());

    let basic_constraints = root
        .basic_constraints()
        .expect("basic constraints extension parses")
        .expect("root has a basic constraints extension");
    assert!(basic_constraints.value.ca, "root must be a CA certificate");

    let key_usage = root
        .key_usage()
        .expect("key usage extension parses")
        .expect("root has a key usage extension");
    assert!(
        key_usage.value.key_cert_sign(),
        "root must be allowed to sign certificates"
    );
}

#[test]
fn root_certificate_pem_never_contains_key_material() {
    let ca = SandboxCertificateAuthority::generate().unwrap();
    let root_pem = ca.root_certificate_pem();

    // The only artifact meant to reach a container is a bare
    // certificate (public trust anchor) — it must parse as a valid
    // X.509 cert and must never carry a PEM-encoded key block.
    assert!(root_pem.starts_with("-----BEGIN CERTIFICATE-----"));
    assert!(!root_pem.contains("PRIVATE KEY"));
    assert!(!root_pem.contains("BEGIN EC PRIVATE KEY"));
    let _ = parse(root_pem); // panics (test-only) if the PEM doesn't parse as a cert
}

#[test]
fn ca_debug_output_never_contains_private_key_material() {
    let ca = SandboxCertificateAuthority::generate().unwrap();
    let root_key = ca.proxy_private_key_pem();

    let debug_ca = format!("{ca:?}");

    assert!(!debug_ca.contains("PRIVATE KEY"));
    assert!(!debug_ca.contains(root_key.expose_secret()));
    assert!(!debug_ca.contains(ca.root_certificate_pem()));
}
