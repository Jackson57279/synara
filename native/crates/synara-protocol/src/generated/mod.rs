//! Files generated from `packages/contracts`. Regenerate with
//! `bun packages/contracts/scripts/export-rpc-manifest.ts native/crates/synara-protocol/src/generated`.

#[rustfmt::skip]
mod rpc_manifest;

pub use rpc_manifest::*;

/// Looks up an RPC by tag.
pub fn rpc(tag: &str) -> Option<&'static RpcInfo> {
    RPCS.iter().find(|info| info.tag == tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_knows_the_client_core() {
        assert!(rpc(tags::ORCHESTRATION_SUBSCRIBE_SHELL).unwrap().stream);
        assert!(rpc(tags::ORCHESTRATION_SUBSCRIBE_THREAD).unwrap().stream);
        assert!(!rpc(tags::ORCHESTRATION_DISPATCH_COMMAND).unwrap().stream);
        assert!(rpc("not.a.real.rpc").is_none());
    }

    #[test]
    fn manifest_is_sorted_and_unique() {
        for pair in RPCS.windows(2) {
            assert!(
                pair[0].tag < pair[1].tag,
                "{} >= {}",
                pair[0].tag,
                pair[1].tag
            );
        }
    }

    #[test]
    fn limits_match_the_spec() {
        assert_eq!(PROTOCOL_EPOCH, 1);
        const { assert!(MIN_REVISION <= MAX_REVISION) };
        const { assert!(STREAM_LIMIT_THREAD <= STREAM_LIMIT_TOTAL) };
        assert!(REQUIRED_CAPABILITIES.contains(&"orchestration.cursor-safe-streams"));
    }
}
