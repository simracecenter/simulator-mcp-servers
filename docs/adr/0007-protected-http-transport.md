# ADR 0007: Opt-In Protected HTTP Transport

- Status: Proposed
- Date: 2026-09-09
- Engineering issue: https://github.com/simracecenter/simulator-mcp-servers/issues/54

## Context

Director needs an authorization boundary before it can manage trusted Windows
companions. ADR 0004's legacy HTTP API is intentionally unauthenticated. The
existing launcher must not silently gain a partial pairing or TLS protocol.

## Proposed Decision

Add `build_protected_router` beside the existing router. It still hosts exactly
one simulator handler. The embedding application supplies an in-memory credential
registry and chooses explicit permitted tool names. There is no wildcard scope.
Credential issuance is a Rust API, never an unauthenticated network endpoint.

Generate bearer secrets from two OS-random UUID v4 values (244 random bits); retain
only SHA-256 token digests server-side. Grants expire within 24 hours and can be
revoked by their separate identifier. Credentials are not serializable or Debug.

Reject Origin-bearing requests and missing, duplicate or invalid credentials
before reading bodies. Reject unclassified methods and tools before dispatch,
including tool calls disguised as notifications. Bind sessions to the individual
credential that initialized them. Protected clients must initialize before GET,
POST operations or DELETE; legacy anonymous streams remain unchanged.

## Limits And Integration Gates

This API is not enabled by the launcher and does not secure existing listeners.
It provides neither TLS nor persistent device identity, pairing, token delivery,
Windows secret storage, lease fencing, operator GUI, or settings-UI protection.
Never expose bearer credentials over off-host plaintext HTTP. A future trusted
TLS listener must own this router, with certificate pinning and GUI pairing.

Revocation currently denies subsequent HTTP requests; it does not cancel a
handler already executing or close an already-open SSE response. Session quotas,
idle expiry and revocation cleanup remain required before listener integration.
Tool permissions do not substitute for leases or simulator-side mutation fencing.

## Validation

Router tests cover denied requests before dispatch, exact tool permissions,
credential revocation, cross-credential session rejection, and Origin/header
ambiguity. Docker is a development/test environment only; deployment stays native
Windows and the existing stdio transport is unchanged.