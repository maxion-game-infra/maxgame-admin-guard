export {
  adminHasAnySiteAccess,
  adminHasFeatureOnSite,
  adminHasSiteOnToken,
  normalizeAdminSiteAccess,
} from './core/access';
export type {
  AdminAuthContext,
  AdminGuardConfig,
  AdminIntrospectionResult,
  AdminSiteAccess,
  AdminTokenClaims,
  CircuitBreakerConfig,
  RequireScope,
} from './core/types';
export { isActiveIntrospection } from './core/types';
export { checkAdminScope, verifyAdminAuth } from './core/authorize';
export type {
  VerifyAdminAuthDeps,
  VerifyAdminAuthInput,
} from './core/authorize';
export { CircuitBreaker } from './core/circuit-breaker';
export type { CircuitState } from './core/circuit-breaker';
export { AdminGuardError } from './core/errors';
export type { AdminGuardErrorStatus } from './core/errors';
export { AdminIntrospectClient } from './core/introspect-client';
export type {
  AdminIntrospectClientConfig,
  IntrospectTransport,
} from './core/introspect-client';
export { AdminJwtVerifier } from './core/jwt-verifier';
export type { AdminJwtVerifierConfig, JwksResolver } from './core/jwt-verifier';
