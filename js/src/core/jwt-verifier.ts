import * as jose from 'jose';

import { AdminGuardError } from './errors';
import type { AdminTokenClaims } from './types';
import { DEFAULT_JWKS_PATH } from './types';

/**
 * A jose-compatible key resolver: `createRemoteJWKSet` and
 * `createLocalJWKSet` both produce this shape. Accepting it directly (rather
 * than only a URL) is what lets tests inject a `createLocalJWKSet` backed by
 * the contract's fixture keypair with zero network and zero module mocking.
 */
export type JwksResolver = Parameters<typeof jose.jwtVerify>[1];

export type AdminJwtVerifierConfig = {
  /** Must byte-equal the token's `iss` claim. */
  issuer: string;
  /** Base URL of the IdP. Required unless `jwks` is supplied directly. */
  idpBaseUrl?: string;
  /** Default `/.well-known/jwks.json`. */
  jwksPath?: string;
  /** Inject to bypass network resolution (tests, or a pre-fetched JWKS). */
  jwks?: JwksResolver;
};

/**
 * Offline verification of admin access tokens: EdDSA signature over the
 * IdP's published JWKS, issuer byte-equality, expiry, and (rule 2) a
 * required `kid` in the header — checked explicitly here because jose's own
 * matching can, depending on the JWKS resolver, still succeed for a header
 * that omits `kid` when exactly one candidate key exists.
 *
 * Does NOT check `type: 'admin'` — callers must do that themselves, so an
 * unexpected token type is a distinguishable rejection rather than folded
 * into "invalid token" (contract case `wrong-type-401`).
 *
 * `aud` is deliberately never passed as a verification constraint (contract
 * rule 2): a token carrying `aud` must still verify.
 */
export class AdminJwtVerifier {
  private readonly issuer: string;
  private readonly jwksUrl: URL | undefined;
  private jwks: JwksResolver | undefined;

  constructor(config: AdminJwtVerifierConfig) {
    this.issuer = (config.issuer ?? '').trim();
    const baseUrl = (config.idpBaseUrl ?? '').trim();

    if (!this.issuer) {
      throw new Error('AdminJwtVerifier: missing issuer');
    }
    if (!config.jwks && !baseUrl) {
      throw new Error(
        'AdminJwtVerifier: missing idpBaseUrl (or provide a jwks resolver directly)',
      );
    }

    if (config.jwks) {
      this.jwks = config.jwks;
    } else {
      this.jwksUrl = new URL(config.jwksPath ?? DEFAULT_JWKS_PATH, baseUrl);
    }
  }

  private getJwks(): JwksResolver {
    // Lazily created (not in the constructor) so createRemoteJWKSet's own
    // internal fetch/cache state is easy to swap out in tests.
    if (!this.jwks) {
      this.jwks = jose.createRemoteJWKSet(this.jwksUrl as URL);
    }
    return this.jwks;
  }

  async verify(rawToken: string): Promise<AdminTokenClaims> {
    let header: jose.ProtectedHeaderParameters;
    try {
      header = jose.decodeProtectedHeader(rawToken);
    } catch {
      throw new AdminGuardError(401, 'Invalid token');
    }

    // Rule 2: kid required. Checked before touching the JWKS so a malformed
    // or kid-less token never triggers a fetch (contract cases
    // `malformed-token-401` / `missing-kid-401`).
    if (!header.kid) {
      throw new AdminGuardError(401, 'Invalid token');
    }

    try {
      const { payload } = await jose.jwtVerify(rawToken, this.getJwks(), {
        issuer: this.issuer,
        algorithms: ['EdDSA'],
      });
      return payload as AdminTokenClaims;
    } catch (err) {
      throw classifyVerifyError(err);
    }
  }
}

/**
 * Rule 5: a key set that cannot be *obtained* — the JWKS endpoint is
 * unreachable, answers non-2xx, or returns something unparseable — is a
 * dependency outage (503); the token is not implicated. A key set that
 * *arrives* and simply lacks the token's `kid`, or any other failure once
 * jose has a key set in hand (bad signature, wrong issuer, expired, wrong
 * alg), is a bad credential (401).
 *
 * This is decided by inspecting jose's error *types*, not message strings
 * (which change between versions and are not part of jose's contract).
 * Every failure jose raises while checking a token it already has a key
 * for is a named `jose.errors.JOSEError` subclass with its own `code`. The
 * JWKS fetch path is the one exception: on a timeout it throws
 * `JWKSTimeout`, and on a non-2xx response or an unparseable body it
 * throws the bare `JOSEError` base class itself (code `ERR_JOSE_GENERIC`)
 * — nothing else in jose throws that base class directly, every other site
 * throws a subclass. A genuine transport/connection failure (DNS,
 * ECONNREFUSED, socket reset) never reaches jose's parsing code at all, so
 * it surfaces as a plain `Error` that is not a `JOSEError` instance.
 */
function classifyVerifyError(err: unknown): AdminGuardError {
  if (err instanceof jose.errors.JWTExpired) {
    return new AdminGuardError(401, 'Token expired');
  }
  if (err instanceof jose.errors.JWKSNoMatchingKey) {
    // The key set arrived; it just doesn't contain this token's kid.
    return new AdminGuardError(401, 'Invalid token');
  }
  if (err instanceof jose.errors.JWKSTimeout) {
    return new AdminGuardError(503, 'Admin IdP key set is unavailable');
  }
  if (err instanceof jose.errors.JOSEError && err.constructor === jose.errors.JOSEError) {
    // The one case where jose throws its base class directly, rather than
    // a named subclass: fetch_jwks.js on a non-2xx response or malformed
    // JSON body.
    return new AdminGuardError(503, 'Admin IdP key set is unavailable');
  }
  if (!(err instanceof jose.errors.JOSEError)) {
    // Didn't reach jose's own error types at all: a transport-level
    // failure while fetching the JWKS (connection refused/reset, DNS).
    return new AdminGuardError(503, 'Admin IdP key set is unavailable');
  }
  return new AdminGuardError(401, 'Invalid token');
}
