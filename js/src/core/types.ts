/** Grants per site, feature keys only (no edit/readonly distinction). */
export type AdminSiteAccess = Record<string, string[]>;

/**
 * Claims carried by an admin access token, as minted by the IdP
 * (`maxgame-admin-auth-server`). camelCase — a cross-system contract the IdP
 * holds fixed (its own `domain/token.rs` doc comment: ADR D3).
 */
export type AdminTokenClaims = {
  sub: string;
  role: string;
  type: string;
  sid?: string;
  siteAccess?: Record<string, unknown>;
  tokenVersion?: number;
  iat?: number;
  exp?: number;
  iss?: string;
  aud?: string;
};

/**
 * `POST /api/v1/oauth/introspect` response shape. Field names (`adminId`,
 * not `sub`) are the IdP's fixed contract, owned by the IdP rather than by
 * any consumer of this library.
 */
export type AdminIntrospectionResult =
  | {
      active: true;
      adminId: string;
      username?: string;
      role: string;
      siteAccess: Record<string, unknown>;
      tokenVersion?: number;
    }
  | {
      active: false;
      reason: string;
    };

export function isActiveIntrospection(
  verdict: AdminIntrospectionResult,
): verdict is Extract<AdminIntrospectionResult, { active: true }> {
  return verdict.active === true;
}

/** Result of a successful admin-auth decision — what guards/middleware set on the request. */
export type AdminAuthContext = {
  adminId: string;
  adminRole: string;
  adminSiteAccess: AdminSiteAccess;
};

/** What a route requires: a super_admin bypass or a site (+ optional feature). */
export type RequireScope =
  | { site: string; feature: string }
  | { site: string; siteOnly: true }
  | null
  | undefined;

export type CircuitBreakerConfig = {
  /** Consecutive failures (from `closed`) before the breaker trips open. */
  failureThreshold: number;
  /** How long the breaker stays open before allowing one trial call. */
  resetTimeoutMs: number;
};

/**
 * Explicit configuration for the whole admin-auth contract. Nothing in this
 * library reads `process.env` — every value here must be supplied by the
 * caller (typically resolved from the host app's own config/env at wiring
 * time).
 */
export type AdminGuardConfig = {
  /** Must byte-equal the token's `iss` claim. */
  issuer: string;
  /** Base URL of the IdP, e.g. `https://admin-auth.maxion.game`. */
  idpBaseUrl: string;
  /** `x-api-key` (or `introspectHeader`) sent on introspect calls. */
  introspectApiKey: string;
  /** Default `/.well-known/jwks.json`. */
  jwksPath?: string;
  /** Default `/api/v1/oauth/introspect`. */
  introspectPath?: string;
  /** Default `x-api-key`. */
  introspectHeader?: string;
  /** Default `['POST', 'PUT', 'PATCH', 'DELETE']`. */
  mutatingMethods?: string[];
  /** Default `{ failureThreshold: 5, resetTimeoutMs: 30_000 }`. */
  breaker?: CircuitBreakerConfig;
  /** Default 3000ms. */
  introspectTimeoutMs?: number;
  /** Default `Date.now`. Inject for deterministic breaker tests. */
  clock?: () => number;
};

export const DEFAULT_JWKS_PATH = '/.well-known/jwks.json';
export const DEFAULT_INTROSPECT_PATH = '/api/v1/oauth/introspect';
export const DEFAULT_INTROSPECT_HEADER = 'x-api-key';
export const DEFAULT_MUTATING_METHODS = ['POST', 'PUT', 'PATCH', 'DELETE'];
export const DEFAULT_BREAKER: CircuitBreakerConfig = {
  failureThreshold: 5,
  resetTimeoutMs: 30_000,
};
export const DEFAULT_INTROSPECT_TIMEOUT_MS = 3000;
