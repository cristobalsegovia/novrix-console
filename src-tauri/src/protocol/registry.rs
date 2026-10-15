//! The [`ProtocolRegistry`] (Requirements 8.2, 8.4).
//!
//! The registry is the Protocol_Engine's catalogue of registered protocols.
//! It accepts complete protocols, rejects incomplete ones (naming the missing
//! operations), and reports which [`ProtocolKind`]s the Connection_Manager may
//! present as selectable options when a user creates a Connection_Profile.
//!
//! A registered protocol is, by construction, a selectable one: there is no
//! path by which an incomplete protocol enters the catalogue, so
//! "registered kinds" and "selectable kinds" are the same set (Property 19).

use crate::domain::{ProtocolKind, RegistrationError};

use super::interface::{Protocol, UNIFORM_OPERATIONS};

/// Catalogue of registered protocols, keyed by [`ProtocolKind`].
///
/// Only protocols that implement every uniform operation ever make it in, so
/// everything the registry holds is also selectable.
#[derive(Default)]
pub struct ProtocolRegistry {
    /// Registered protocols in registration order. Insertion order is kept so
    /// [`selectable_kinds`](ProtocolRegistry::selectable_kinds) is deterministic.
    protocols: Vec<Box<dyn Protocol>>,
}

impl ProtocolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            protocols: Vec::new(),
        }
    }

    /// Register a protocol through the uniform interface.
    ///
    /// Rejects a protocol that does not implement every operation defined by
    /// the uniform interface, returning a [`RegistrationError`] that names the
    /// missing operations; a rejected protocol is not stored and therefore
    /// never appears in [`selectable_kinds`](ProtocolRegistry::selectable_kinds)
    /// (Requirement 8.4).
    ///
    /// Registering a kind that is already present replaces the prior
    /// registration in place, keeping its position in the selectable order.
    pub fn register(&mut self, p: Box<dyn Protocol>) -> Result<(), RegistrationError> {
        let missing = missing_operations(p.as_ref());
        if !missing.is_empty() {
            return Err(RegistrationError {
                protocol: protocol_label(p.kind()),
                missing_operations: missing,
            });
        }

        let kind = p.kind();
        match self.protocols.iter_mut().find(|existing| existing.kind() == kind) {
            Some(slot) => *slot = p,
            None => self.protocols.push(p),
        }
        Ok(())
    }

    /// The protocols available as selectable options for profile creation
    /// (Requirement 8.2), in registration order.
    ///
    /// Equivalent to the set of successfully registered kinds: incomplete
    /// protocols were rejected by [`register`](ProtocolRegistry::register) and
    /// are absent here.
    pub fn selectable_kinds(&self) -> Vec<ProtocolKind> {
        self.protocols.iter().map(|p| p.kind()).collect()
    }

    /// Whether a protocol of the given kind is currently registered.
    pub fn is_registered(&self, kind: ProtocolKind) -> bool {
        self.protocols.iter().any(|p| p.kind() == kind)
    }

    /// The number of registered protocols.
    pub fn len(&self) -> usize {
        self.protocols.len()
    }

    /// Whether the registry holds no protocols.
    pub fn is_empty(&self) -> bool {
        self.protocols.is_empty()
    }
}

/// The uniform operations a protocol fails to implement, in the canonical order
/// of [`UNIFORM_OPERATIONS`]. An empty result means the protocol is complete.
fn missing_operations(p: &dyn Protocol) -> Vec<String> {
    let supported = p.supported_operations();
    UNIFORM_OPERATIONS
        .iter()
        .filter(|op| !supported.contains(*op))
        .map(|op| (*op).to_string())
        .collect()
}

/// A stable, human-readable label for a protocol kind, used in registration
/// errors.
fn protocol_label(kind: ProtocolKind) -> String {
    match kind {
        ProtocolKind::Ssh => "ssh",
        ProtocolKind::Telnet => "telnet",
        ProtocolKind::Serial => "serial",
        ProtocolKind::Rdp => "rdp",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ProtocolError, SessionState};
    use crate::protocol::interface::{CredentialHandle, ResolvedProfile, SessionConn};
    use async_trait::async_trait;

    /// A minimal in-memory protocol used only to exercise the registry. It
    /// never opens anything; `establish` is unreachable in these tests.
    struct FakeProtocol {
        kind: ProtocolKind,
        supported: Vec<&'static str>,
    }

    impl FakeProtocol {
        /// A complete protocol of the given kind.
        fn complete(kind: ProtocolKind) -> Self {
            Self {
                kind,
                supported: UNIFORM_OPERATIONS.to_vec(),
            }
        }

        /// A protocol missing the named operations.
        fn missing(kind: ProtocolKind, omit: &[&str]) -> Self {
            let supported = UNIFORM_OPERATIONS
                .iter()
                .copied()
                .filter(|op| !omit.contains(op))
                .collect();
            Self { kind, supported }
        }
    }

    #[async_trait]
    impl Protocol for FakeProtocol {
        fn kind(&self) -> ProtocolKind {
            self.kind
        }

        fn supported_operations(&self) -> &[&'static str] {
            &self.supported
        }

        async fn establish(
            &self,
            _cfg: &ResolvedProfile,
            _creds: CredentialHandle,
        ) -> Result<Box<dyn SessionConn>, ProtocolError> {
            unreachable!("registry tests never establish a session")
        }
    }

    // A no-op SessionConn kept here to prove the trait is object-safe behind
    // `Box<dyn SessionConn>`; the full behavioral tests live in tasks 7.2–7.5.
    #[allow(dead_code)]
    struct NoopConn;

    #[async_trait]
    impl SessionConn for NoopConn {
        async fn send(&mut self, _data: &[u8]) -> Result<(), ProtocolError> {
            Ok(())
        }
        async fn recv(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
            Ok(None)
        }
        async fn resize(&mut self, _cols: u16, _rows: u16) -> Result<(), ProtocolError> {
            Ok(())
        }
        async fn close(&mut self) -> Result<(), ProtocolError> {
            Ok(())
        }
        fn state(&self) -> SessionState {
            SessionState::Disconnected
        }
    }

    #[test]
    fn complete_protocol_registers_and_becomes_selectable() {
        let mut registry = ProtocolRegistry::new();
        registry
            .register(Box::new(FakeProtocol::complete(ProtocolKind::Ssh)))
            .expect("complete protocol should register");

        assert_eq!(registry.selectable_kinds(), vec![ProtocolKind::Ssh]);
        assert!(registry.is_registered(ProtocolKind::Ssh));
    }

    #[test]
    fn registration_order_is_preserved() {
        let mut registry = ProtocolRegistry::new();
        for kind in [ProtocolKind::Telnet, ProtocolKind::Ssh, ProtocolKind::Rdp] {
            registry
                .register(Box::new(FakeProtocol::complete(kind)))
                .unwrap();
        }
        assert_eq!(
            registry.selectable_kinds(),
            vec![ProtocolKind::Telnet, ProtocolKind::Ssh, ProtocolKind::Rdp]
        );
    }

    #[test]
    fn incomplete_protocol_is_rejected_and_excluded() {
        let mut registry = ProtocolRegistry::new();
        let err = registry
            .register(Box::new(FakeProtocol::missing(
                ProtocolKind::Serial,
                &["recv", "resize"],
            )))
            .expect_err("incomplete protocol must be rejected");

        assert_eq!(err.protocol, "serial");
        assert_eq!(err.missing_operations, vec!["recv", "resize"]);
        // Rejected => not selectable (Requirement 8.4).
        assert!(registry.selectable_kinds().is_empty());
        assert!(!registry.is_registered(ProtocolKind::Serial));
    }

    #[test]
    fn re_registering_a_kind_replaces_in_place() {
        let mut registry = ProtocolRegistry::new();
        registry
            .register(Box::new(FakeProtocol::complete(ProtocolKind::Ssh)))
            .unwrap();
        registry
            .register(Box::new(FakeProtocol::complete(ProtocolKind::Telnet)))
            .unwrap();
        registry
            .register(Box::new(FakeProtocol::complete(ProtocolKind::Ssh)))
            .unwrap();

        assert_eq!(registry.len(), 2);
        assert_eq!(
            registry.selectable_kinds(),
            vec![ProtocolKind::Ssh, ProtocolKind::Telnet]
        );
    }

    // ---------------------------------------------------------------------
    // Unit test — 7.5: the uniform-trait structural contract (Requirement 8.1)
    // ---------------------------------------------------------------------

    /// Every Supported_Protocol kind must have a type that implements the
    /// uniform [`Protocol`] trait, and a complete implementation must advertise
    /// exactly the uniform operation set.
    ///
    /// This is a *structural* check (task 7.5): the real SSH/Telnet/Serial/RDP
    /// implementations (tasks 8–11) require injected transport dependencies, so
    /// rather than construct live connectors here we verify the property at the
    /// level the uniform interface actually guarantees — that for each
    /// [`ProtocolKind`] a `Box<dyn Protocol>` exists, reports that kind, and (as
    /// a complete protocol) exposes `supported_operations() == UNIFORM_OPERATIONS`.
    /// The `register`/`selectable_kinds` round-trip confirms the registry accepts
    /// one uniform implementation per supported kind.
    #[test]
    fn every_supported_kind_has_a_uniform_protocol_impl() {
        // The full set of Supported_Protocol kinds the uniform interface covers.
        let all_kinds = [
            ProtocolKind::Ssh,
            ProtocolKind::Telnet,
            ProtocolKind::Serial,
            ProtocolKind::Rdp,
        ];

        let mut registry = ProtocolRegistry::new();
        for kind in all_kinds {
            // A `Box<dyn Protocol>` exists for this kind: the trait is
            // object-safe and implemented (compiles) for every supported kind.
            let protocol: Box<dyn Protocol> = Box::new(FakeProtocol::complete(kind));

            // It names its own kind through the uniform `kind()` operation.
            assert_eq!(protocol.kind(), kind);

            // A complete protocol implements the whole uniform interface — the
            // exact operation set, in the canonical order.
            assert_eq!(protocol.supported_operations(), UNIFORM_OPERATIONS);

            // And it registers as a complete, uniform implementation.
            registry
                .register(protocol)
                .expect("a complete uniform protocol must register");
        }

        // One uniform implementation per supported kind is now selectable.
        assert_eq!(registry.selectable_kinds(), all_kinds.to_vec());
    }

    // ---------------------------------------------------------------------
    // Property-based tests (tasks 7.2, 7.4). The registry surface is sync, so
    // these drive it directly. Each uses >= 100 iterations and reuses the
    // in-memory `FakeProtocol` above so nothing opens a real transport.
    // ---------------------------------------------------------------------
    mod properties {
        use super::*;
        use proptest::prelude::*;
        use std::collections::HashSet;

        /// The four Supported_Protocol kinds, indexable for strategies.
        const ALL_KINDS: [ProtocolKind; 4] = [
            ProtocolKind::Ssh,
            ProtocolKind::Telnet,
            ProtocolKind::Serial,
            ProtocolKind::Rdp,
        ];

        /// A strategy producing an arbitrary ordered sequence of *distinct*
        /// protocol kinds: a shuffle of a non-empty subset of [`ALL_KINDS`].
        ///
        /// We generate a boolean inclusion mask over the four kinds (rejecting
        /// the empty mask) and then an arbitrary permutation, so both the
        /// *subset* and the *registration order* vary across cases.
        fn distinct_kind_sequence() -> impl Strategy<Value = Vec<ProtocolKind>> {
            (
                proptest::collection::vec(any::<bool>(), ALL_KINDS.len()),
                Just(ALL_KINDS.to_vec()).prop_shuffle(),
            )
                .prop_filter_map(
                    "at least one kind must be selected",
                    |(mask, shuffled)| {
                        let selected: Vec<ProtocolKind> = shuffled
                            .into_iter()
                            .filter(|k| mask[ALL_KINDS.iter().position(|x| x == k).unwrap()])
                            .collect();
                        if selected.is_empty() {
                            None
                        } else {
                            Some(selected)
                        }
                    },
                )
        }

        /// A strategy producing a non-empty subset of [`UNIFORM_OPERATIONS`] to
        /// omit, so the resulting protocol is always genuinely incomplete.
        fn non_empty_omitted_operations() -> impl Strategy<Value = Vec<&'static str>> {
            proptest::collection::vec(any::<bool>(), UNIFORM_OPERATIONS.len()).prop_filter_map(
                "at least one operation must be omitted",
                |mask| {
                    let omitted: Vec<&'static str> = UNIFORM_OPERATIONS
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| mask[*i])
                        .map(|(_, op)| *op)
                        .collect();
                    if omitted.is_empty() {
                        None
                    } else {
                        Some(omitted)
                    }
                },
            )
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            // Feature: remote-connection-manager, Property 19: Registered protocols equal selectable options
            //
            // *For any* set of successfully registered protocols, the set of
            // protocols offered as selectable options during profile creation
            // equals exactly that registered set.
            //
            // Validates: Requirements 8.2
            #[test]
            fn registered_protocols_equal_selectable_options(
                kinds in distinct_kind_sequence(),
            ) {
                let mut registry = ProtocolRegistry::new();
                for kind in &kinds {
                    registry
                        .register(Box::new(FakeProtocol::complete(*kind)))
                        .expect("a complete protocol must register");
                }

                let selectable = registry.selectable_kinds();

                // Exactly the registered set, in registration order.
                prop_assert_eq!(&selectable, &kinds);

                // Set equality, order aside: selectable == registered, no more,
                // no fewer, no duplicates.
                let registered_set: HashSet<ProtocolKind> = kinds.iter().copied().collect();
                let selectable_set: HashSet<ProtocolKind> = selectable.iter().copied().collect();
                prop_assert_eq!(selectable_set, registered_set);
                prop_assert_eq!(selectable.len(), kinds.len());
            }

            // Feature: remote-connection-manager, Property 21: Incomplete protocol registration is rejected and excluded
            //
            // *For any* protocol that does not implement every operation of the
            // uniform interface, registration is rejected with an error naming
            // the missing operations, and the protocol is absent from the
            // selectable options.
            //
            // Validates: Requirements 8.4
            #[test]
            fn incomplete_registration_is_rejected_and_excluded(
                kind_idx in 0usize..ALL_KINDS.len(),
                omitted in non_empty_omitted_operations(),
            ) {
                let kind = ALL_KINDS[kind_idx];
                let mut registry = ProtocolRegistry::new();

                let err = registry
                    .register(Box::new(FakeProtocol::missing(kind, &omitted)))
                    .expect_err("an incomplete protocol must be rejected");

                // The error names the rejected protocol...
                prop_assert_eq!(&err.protocol, &protocol_label(kind));

                // ...and names exactly the missing operations, in the canonical
                // UNIFORM_OPERATIONS order (not generation order).
                let expected_missing: Vec<String> = UNIFORM_OPERATIONS
                    .iter()
                    .filter(|op| omitted.contains(*op))
                    .map(|op| (*op).to_string())
                    .collect();
                prop_assert_eq!(&err.missing_operations, &expected_missing);
                prop_assert!(!err.missing_operations.is_empty());

                // The rejected protocol is absent from the selectable options.
                prop_assert!(!registry.is_registered(kind));
                prop_assert!(!registry.selectable_kinds().contains(&kind));
                prop_assert!(registry.selectable_kinds().is_empty());
            }
        }
    }
}
