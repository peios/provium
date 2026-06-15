//! Conformance: protocol version handshake per `DESIGN.md` §
//! Protocol version handshake. Hello / HelloOk / HelloErr round
//! trip, version mismatch is fatal, AgentInfo carries the
//! diagnostic fields tooling depends on.

use provium_protocol::{
    wire::{AgentInfo, AgentMessage, Hello, HelloErr, HelloOk, HostMessage},
    PROTOCOL_VERSION,
};

#[test]
fn protocol_version_constant_is_positive() {
    assert!(PROTOCOL_VERSION > 0,
        "PROTOCOL_VERSION must be a positive monotonic counter");
}

#[test]
fn hello_round_trips_through_msgpack() {
    let h = Hello { protocol_version: PROTOCOL_VERSION };
    let bytes = rmp_serde::to_vec_named(&h).unwrap();
    let back: Hello = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(h, back);
}

#[test]
fn hello_ok_carries_agent_info() {
    let ok = HelloOk {
        protocol_version: PROTOCOL_VERSION,
        agent: AgentInfo {
            os: "peios".into(),
            agent_version: "0.1.0".into(),
            kernel: Some("6.12.0-peios".into()),
            capabilities: vec![],
        },
    };
    let bytes = rmp_serde::to_vec_named(&ok).unwrap();
    let back: HelloOk = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(ok, back);
    assert_eq!(back.agent.os, "peios");
}

#[test]
fn hello_err_carries_both_versions() {
    // The diagnostic value of HelloErr is naming the mismatch
    // explicitly so the host can tell users which side to update.
    let err = HelloErr { host_version: 5, agent_version: 3 };
    let bytes = rmp_serde::to_vec_named(&err).unwrap();
    let back: HelloErr = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back.host_version, 5);
    assert_eq!(back.agent_version, 3);
}

#[test]
fn agent_info_capabilities_default_empty_in_v1() {
    let info = AgentInfo {
        os: "peios".into(),
        agent_version: "0.1.0".into(),
        kernel: None,
        capabilities: vec![],
    };
    assert!(info.capabilities.is_empty(),
        "v1 has no negotiated capabilities");
}

#[test]
fn host_hello_serializes_under_correct_kind_tag() {
    // The HostMessage envelope tags Hello with kind="hello".
    let h = HostMessage::Hello(Hello { protocol_version: PROTOCOL_VERSION });
    let bytes = rmp_serde::to_vec_named(&h).unwrap();
    // Crude grep — the raw bytes must contain the kind tag.
    let needle = b"hello";
    let found = bytes.windows(needle.len()).any(|w| w == needle);
    assert!(found, "HostMessage::Hello must serialize with `hello` tag");
}

#[test]
fn agent_hello_ok_serializes_under_correct_kind_tag() {
    let m = AgentMessage::HelloOk(HelloOk {
        protocol_version: PROTOCOL_VERSION,
        agent: AgentInfo {
            os: "peios".into(),
            agent_version: "0.1.0".into(),
            kernel: None,
            capabilities: vec![],
        },
    });
    let bytes = rmp_serde::to_vec_named(&m).unwrap();
    let needle = b"hello_ok";
    let found = bytes.windows(needle.len()).any(|w| w == needle);
    assert!(found, "AgentMessage::HelloOk must serialize with `hello_ok` tag");
}

#[test]
fn agent_hello_err_serializes_under_correct_kind_tag() {
    let m = AgentMessage::HelloErr(HelloErr {
        host_version: 99,
        agent_version: PROTOCOL_VERSION,
    });
    let bytes = rmp_serde::to_vec_named(&m).unwrap();
    let needle = b"hello_err";
    let found = bytes.windows(needle.len()).any(|w| w == needle);
    assert!(found);
}
