//! Fixture states for the Computers scenes (`--scene computers`,
//! `computers-remote`, `computers-pairing`, `computers-light`,
//! `computers-light-paired`).

use chrono::{Duration, Utc};
use zlo_client::environment_access::{
    DevicePairedBy, DeviceSummary, EnvironmentDescriptor, PairingMethod,
};
use zlo_client::environments::{Active, Route, RouteKind, SavedEnvironment};
use zlo_client::tailnet::{Discovery, Found, Peer};

use crate::onboarding::gateway::Computer;
use crate::remote::pairing::Issued;
use crate::state::State;

pub const STUDIO: &str = "0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10";
pub const MINI: &str = "6a1d3c4b-0000-4a51-9b7e-6b1c8f1f2a11";
pub const OLD: &str = "7b2e4d5c-0000-4a51-9b7e-6b1c8f1f2a12";

fn env(id: &str, label: &str, dns: &str, legacy: bool) -> SavedEnvironment {
    SavedEnvironment {
        environment_id: id.into(),
        label: label.into(),
        routes: vec![Route {
            kind: RouteKind::Tailnet,
            url: format!("https://{dns}"),
            remote_port: None,
        }],
        legacy_token: legacy,
    }
}

fn descriptor(id: &str, label: &str, tailnet: Option<&str>) -> EnvironmentDescriptor {
    EnvironmentDescriptor {
        environment_id: id.into(),
        label: label.into(),
        server_version: "0.41.0".into(),
        protocol_version: 1,
        pairing: vec![PairingMethod::OneTimeCode, PairingMethod::TailnetIdentity],
        tailnet_url: tailnet.map(str::to_string),
        build_commit: None,
        self_update: false,
    }
}

fn peer(dns: &str, online: bool) -> Peer {
    Peer {
        dns_name: dns.into(),
        host_name: dns.split('.').next().unwrap_or(dns).into(),
        online,
    }
}

fn device(
    id: &str,
    label: &str,
    by: DevicePairedBy,
    current: bool,
    revoked: bool,
) -> DeviceSummary {
    DeviceSummary {
        device_id: id.into(),
        label: label.into(),
        os: "macOS".into(),
        app_version: "0.1.0".into(),
        paired_by: by,
        created_at: Utc::now() - Duration::days(3),
        last_seen_at: Some(Utc::now() - Duration::minutes(5)),
        revoked_at: revoked.then(|| Utc::now() - Duration::days(1)),
        current,
    }
}

fn base(s: &mut State) {
    let r = &mut s.remote;
    r.registry.environments = vec![
        env(STUDIO, "Studio", "studio.tail9915dd.ts.net", false),
        env(MINI, "Mac mini", "mini.tail9915dd.ts.net", false),
        env(OLD, "Old laptop", "old.tail9915dd.ts.net", true),
    ];
    r.problems.insert(
        OLD.into(),
        "Old laptop turned this Mac's token down (it may have been revoked). Pair it again in Settings → Computers."
            .into(),
    );
    r.page.tailscale = Some(true);
    r.page.discovery = Some(Discovery {
        peers: vec![
            peer("studio.tail9915dd.ts.net", true),
            peer("mini.tail9915dd.ts.net", false),
            peer("office.tail9915dd.ts.net", true),
        ],
        found: vec![Found {
            peer: peer("office.tail9915dd.ts.net", true),
            descriptor: descriptor(
                "9c3f5e6d-0000-4a51-9b7e-6b1c8f1f2a13",
                "Office",
                Some("https://office.tail9915dd.ts.net"),
            ),
        }],
    });
    r.page.devices = Some(Ok(vec![
        device(
            "0123456789abcdef0123456789abcdef",
            "Lorik's MacBook Air",
            DevicePairedBy::TailnetIdentity,
            false,
            false,
        ),
        device(
            "fedcba9876543210fedcba9876543210",
            "Old laptop",
            DevicePairedBy::OneTimeCode,
            false,
            true,
        ),
    ]));
}

/// This Mac in use, its backend shared on the tailnet.
pub fn computers(s: &mut State) {
    base(s);
    s.remote.descriptor = Some(descriptor(
        "11111111-2222-4333-8444-555555555555",
        "Lorik's MacBook Pro",
        Some("https://loriks-macbook-pro.tail9915dd.ts.net"),
    ));
}

/// Working on Studio from a Mac that runs no backend of its own.
pub fn remote(s: &mut State) {
    base(s);
    s.remote.registry.run_local_backend = false;
    s.remote.registry.active = Active::Environment(STUDIO.into());
    s.onboarding.computer = Computer::remote(STUDIO, "Studio");
    s.remote.descriptor = Some(descriptor(
        STUDIO,
        "Studio",
        Some("https://studio.tail9915dd.ts.net"),
    ));
    if let Some(Ok(devices)) = &mut s.remote.page.devices {
        devices[0].current = true;
    }
    s.remote.page.issued = Some(Issued {
        code: "7KQ2-M9XD-4TRA".into(),
        expires_at: Utc::now() + Duration::minutes(5),
        link: "https://studio.tail9915dd.ts.net/pair#code=7KQ2-M9XD-4TRA".into(),
    });
}

/// A one-click refusal: the code field takes over for Office.
pub fn pairing(s: &mut State) {
    computers(s);
    let page = &mut s.remote.page;
    page.pair_target = Some(("https://office.tail9915dd.ts.net".into(), "Office".into()));
    page.input = rune::TextEdit::new("7KQ2-M9XD");
    page.error = Some(
        "Office belongs to someone else on your tailnet. Ask its owner for a pairing code.".into(),
    );
}

/// A Light bundle's first launch: no IronClaw here, no computer yet, a
/// computer over SSH being typed.
pub fn light(s: &mut State) {
    s.remote.app_only = true;
    s.remote.registry.environments.clear();
    s.remote.registry.active = Active::ThisMac;
    s.remote.page.tailscale = Some(false);
    s.remote.page.ssh_input = rune::TextEdit::new("lorik@studio.local");
    s.conn = crate::state::Conn::Failed {
        message: crate::remote::connect::NEEDS_COMPUTER.into(),
        retry_in: None,
    };
}

/// Settings → Computers in a Light bundle, Studio paired over SSH.
pub fn light_paired(s: &mut State) {
    remote(s);
    s.remote.app_only = true;
    if let Some(studio) = s
        .remote
        .registry
        .environments
        .iter_mut()
        .find(|e| e.environment_id == STUDIO)
        && let Ok(target) = zlo_client::ssh::parse_target("lorik@studio.local")
    {
        studio.routes = vec![Route::ssh(&target, 3000)];
    }
}
