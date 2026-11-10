//! Internal per-tenant CA for the sandbox egress proxy (W5 — design doc
//! `docs/internal/plans/2026-07-26-sandbox-credential-firewall-design.md` §4).
//!
//! Generates a root key/cert pair in memory at construction. The public root
//! certificate can be installed in an untrusted user container. Managed
//! egress may serialize the root key only into its proxy-private material
//! directory so the trusted proxy can issue leaves; the key is never exposed
//! to the user container or logs.

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use secrecy::SecretString;
use time::{Duration as CertValidityDuration, OffsetDateTime};

use ironclaw_host_api::process::RuntimeProcessError;

/// Clock-skew allowance applied to `not_before` so a cert issued a moment
/// ago is never rejected as "not yet valid" by a container whose clock
/// runs slightly behind the host's.
const NOT_BEFORE_SKEW: CertValidityDuration = CertValidityDuration::minutes(5);

/// Root CA validity window. The root is regenerated fresh in memory on
/// every process start (see the module doc and [`SandboxCertificateAuthority::generate`]),
/// so this only has to comfortably outlive one process's lifetime.
const ROOT_VALIDITY: CertValidityDuration = CertValidityDuration::days(30);

/// In-memory root CA. Callers can export the public root certificate, and the
/// signing key only as a redacted secret for the trusted proxy sidecar.
pub(crate) struct SandboxCertificateAuthority {
    root_cert_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl std::fmt::Debug for SandboxCertificateAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omit `issuer`: it carries the root private key, which
        // never belongs in a log line.
        formatter
            .debug_struct("SandboxCertificateAuthority")
            .finish_non_exhaustive()
    }
}

impl SandboxCertificateAuthority {
    /// Generates a fresh root key + self-signed CA certificate in memory and
    /// returns the CA. The root private key remains in process memory except
    /// while an in-memory archive is uploaded directly to a trusted proxy
    /// sidecar.
    pub(crate) fn generate() -> Result<Self, RuntimeProcessError> {
        let mut params = CertificateParams::new(Vec::new()).map_err(ca_error)?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "IronClaw Sandbox Egress CA");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "IronClaw");
        params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        params.key_usages.push(KeyUsagePurpose::CrlSign);
        params.key_usages.push(KeyUsagePurpose::DigitalSignature);
        let now = OffsetDateTime::now_utc();
        params.not_before = now
            .checked_sub(NOT_BEFORE_SKEW)
            .ok_or_else(|| ca_range_error("root not_before"))?;
        params.not_after = now
            .checked_add(ROOT_VALIDITY)
            .ok_or_else(|| ca_range_error("root not_after"))?;

        let root_key = KeyPair::generate().map_err(ca_error)?;
        // `self_signed` only borrows `params` — signing happens before
        // `params` moves into `Issuer::new` just below.
        let root_cert = params.self_signed(&root_key).map_err(ca_error)?;
        let root_cert_pem = root_cert.pem();
        let issuer = Issuer::new(params, root_key);

        Ok(Self {
            root_cert_pem,
            issuer,
        })
    }

    /// The CA's public trust anchor. Managed egress binds this file read-only
    /// into the user container and points common TLS clients at it.
    pub(crate) fn root_certificate_pem(&self) -> &str {
        &self.root_cert_pem
    }

    /// The signing key consumed only by the trusted proxy sidecar.
    ///
    /// Wrapping the serialized value keeps debug output redacted and zeroizes
    /// the allocation when the caller finishes writing its private material.
    pub(crate) fn proxy_private_key_pem(&self) -> SecretString {
        SecretString::new(self.issuer.key().serialize_pem().into_boxed_str())
    }
}

fn ca_error(error: rcgen::Error) -> RuntimeProcessError {
    RuntimeProcessError::ExecutionFailed(format!("sandbox CA: {error}"))
}

fn ca_range_error(field: &str) -> RuntimeProcessError {
    RuntimeProcessError::ExecutionFailed(format!(
        "sandbox CA: {field} computation overflowed the valid date range"
    ))
}

#[cfg(test)]
mod tests;
