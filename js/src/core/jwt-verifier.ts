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
      if ((err as { code?: string })?.code === 'ERR_JWT_EXPIRED') {
        throw new AdminGuardError(401, 'Token expired');
      }
      throw new AdminGuardError(401, 'Invalid token');
    }
  }
}
