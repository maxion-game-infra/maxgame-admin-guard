/**
 * Drives every case in `../../contract/cases.json` through the real core
 * verifier (`AdminJwtVerifier` + `AdminIntrospectClient` + `verifyAdminAuth`
 * + `checkAdminScope`) — not a reimplementation of the rules, the actual
 * production code path, with introspection stubbed per case and a fake
 * clock for the breaker's `sequence` cases. See contract/README.md.
 */
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import * as jose from 'jose';
import { describe, expect, it, vi } from 'vitest';

import { checkAdminScope, verifyAdminAuth } from '../src/core/authorize';
import { AdminGuardError } from '../src/core/errors';
import { AdminIntrospectClient } from '../src/core/introspect-client';
import type { IntrospectTransport } from '../src/core/introspect-client';
import { AdminJwtVerifier } from '../src/core/jwt-verifier';
import type { JwksResolver } from '../src/core/jwt-verifier';
import type { RequireScope } from '../src/core/types';

const dirname = path.dirname(fileURLToPath(import.meta.url));
const CONTRACT_DIR = path.resolve(dirname, '../../contract');

function loadJson(relativePath: string): any {
  return JSON.parse(readFileSync(path.join(CONTRACT_DIR, relativePath), 'utf8'));
}

const contract = loadJson('cases.json');
const jwksDoc = loadJson('fixtures/jwks.json');
const signingPrivateJwk = loadJson('fixtures/signing-key.private.jwk.json');
const wrongPrivateJwk = loadJson('fixtures/wrong-key.private.jwk.json');

const baseConfig = contract.config as {
  issuer: string;
  jwksPath: string;
  introspectPath: string;
  introspectHeader: string;
  breaker: { failureThreshold: number; openMs: number };
  mutatingMethods: string[];
};

type TokenSpec = {
  sub?: string;
  role: string;
  type: string;
  siteAccess: Record<string, unknown>;
  valid?: boolean;
  expired?: boolean;
  signWith?: string;
  omitKid?: boolean;
  kid?: string;
  iss?: string;
  aud?: string;
  tokenVersion?: number;
};

async function mintToken(spec: TokenSpec): Promise<string> {
  const privateJwk = spec.signWith === 'wrong-key' ? wrongPrivateJwk : signingPrivateJwk;
  const privateKey = await jose.importJWK(privateJwk, 'EdDSA');

  const header: jose.JWTHeaderParameters = { alg: 'EdDSA' };
  if (!spec.omitKid) {
    header.kid = spec.kid ?? signingPrivateJwk.kid;
  }

  const payload: Record<string, unknown> = {
    sub: spec.sub ?? '6c7b84de-9e3d-40ef-97eb-6bd47ae170a5',
    role: spec.role,
    type: spec.type,
    siteAccess: spec.siteAccess,
    tokenVersion: spec.tokenVersion ?? 0,
  };
  if (spec.aud) {
    payload.aud = spec.aud;
  }

  let signer = new jose.SignJWT(payload)
    .setProtectedHeader(header)
    .setIssuer(spec.iss ?? baseConfig.issuer)
    .setIssuedAt();

  signer = spec.expired
    ? signer.setExpirationTime(Math.floor(Date.now() / 1000) - 3600)
    : signer.setExpirationTime('15m');

  return signer.sign(privateKey as Parameters<jose.SignJWT['sign']>[0]);
}

async function tokenForCase(kase: {
  rawToken?: string;
  token?: TokenSpec | null;
}): Promise<string | null> {
  if (kase.rawToken !== undefined) {
    return kase.rawToken;
  }
  if (!kase.token) {
    return null;
  }
  return mintToken(kase.token);
}

/** Everything one case (or one step of a sequence case) can stub the introspect response to. */
type IntrospectSpec =
  | { transport: 'timeout' }
  | { transport: 'http'; status: number }
  | { transport: 'malformed' }
  | Record<string, unknown> // an {active:true,...} or {active:false,reason} verdict, sent as-is
  | null;

function specToTransportResult(spec: IntrospectSpec): { status: number; body: unknown } {
  if (!spec) {
    throw new Error('contract test bug: no introspect stub configured for this call');
  }
  if ('transport' in spec) {
    if (spec.transport === 'timeout') {
      throw new Error('ETIMEDOUT');
    }
    if (spec.transport === 'http') {
      return { status: (spec as { status: number }).status, body: {} };
    }
    if (spec.transport === 'malformed') {
      throw new Error('Unexpected token in JSON at position 0');
    }
  }
  return { status: 200, body: spec };
}

type HarnessOutcome = {
  status: number;
  reason?: string;
  introspectCalled: boolean;
  jwksFetched: boolean;
  context?: { adminId: string; adminRole: string; adminSiteAccessPresent: boolean };
};

async function runRequest(
  method: string,
  rawToken: string | null,
  require: RequireScope,
  jwtVerifier: AdminJwtVerifier,
  introspectClient: AdminIntrospectClient,
  transportSpy: ReturnType<typeof vi.fn>,
  jwksSpy: ReturnType<typeof vi.fn>,
  mutatingMethods: string[],
): Promise<HarnessOutcome> {
  const transportCallsBefore = transportSpy.mock.calls.length;
  const jwksCallsBefore = jwksSpy.mock.calls.length;

  try {
    const ctx = await verifyAdminAuth(
      {
        method,
        authorizationHeader: rawToken ? `Bearer ${rawToken}` : null,
      },
      { jwtVerifier, introspectClient, mutatingMethods },
    );
    checkAdminScope(ctx, require);
    return {
      status: 200,
      introspectCalled: transportSpy.mock.calls.length > transportCallsBefore,
      jwksFetched: jwksSpy.mock.calls.length > jwksCallsBefore,
      context: {
        adminId: ctx.adminId,
        adminRole: ctx.adminRole,
        adminSiteAccessPresent: ctx.adminSiteAccess !== undefined && ctx.adminSiteAccess !== null,
      },
    };
  } catch (err) {
    const status = err instanceof AdminGuardError ? err.status : 500;
    return {
      status,
      reason: err instanceof Error ? err.message : String(err),
      introspectCalled: transportSpy.mock.calls.length > transportCallsBefore,
      jwksFetched: jwksSpy.mock.calls.length > jwksCallsBefore,
    };
  }
}

function assertOutcome(expectSpec: Record<string, unknown>, outcome: HarnessOutcome): void {
  if (expectSpec.status !== undefined) {
    expect(outcome.status).toBe(expectSpec.status);
  }
  if (expectSpec.statusIn !== undefined) {
    expect(expectSpec.statusIn as number[]).toContain(outcome.status);
  }
  if (expectSpec.notStatus !== undefined) {
    expect(outcome.status).not.toBe(expectSpec.notStatus);
  }
  if (expectSpec.introspectCalled !== undefined) {
    expect(outcome.introspectCalled).toBe(expectSpec.introspectCalled);
  }
  if (expectSpec.jwksFetched !== undefined) {
    expect(outcome.jwksFetched).toBe(expectSpec.jwksFetched);
  }
  if (expectSpec.reasonContains !== undefined) {
    expect(outcome.reason ?? '').toContain(expectSpec.reasonContains as string);
  }
  if (expectSpec.adminIdSet !== undefined) {
    expect(Boolean(outcome.context?.adminId)).toBe(expectSpec.adminIdSet);
  }
  if (expectSpec.context !== undefined) {
    expect(outcome.context).toEqual(expectSpec.context);
  }
}

describe('admin-auth contract', () => {
  for (const kase of contract.cases as Array<Record<string, any>>) {
    it(`${kase.id}: ${kase.why}`, async () => {
      const config = { ...baseConfig, ...(kase.config ?? {}) };
      const clock = { now: 0 };

      const jwksSpy = vi.fn();
      type LocalJwksFn = ReturnType<typeof jose.createLocalJWKSet>;
      const baseResolver: LocalJwksFn = jose.createLocalJWKSet(jwksDoc);
      const spiedJwks = (async (...args: Parameters<LocalJwksFn>) => {
        jwksSpy();
        return baseResolver(...args);
      }) as JwksResolver;

      let currentIntrospectSpec: IntrospectSpec = kase.introspect ?? null;
      const transportSpy = vi.fn();
      const transport: IntrospectTransport = async (token: string) => {
        transportSpy(token);
        return specToTransportResult(currentIntrospectSpec);
      };

      let jwtVerifier: AdminJwtVerifier;
      let introspectClient: AdminIntrospectClient;
      try {
        jwtVerifier = new AdminJwtVerifier({ issuer: config.issuer, jwks: spiedJwks });
        introspectClient = new AdminIntrospectClient({
          idpBaseUrl: config.idpBaseUrl ?? 'http://idp.invalid',
          introspectApiKey: config.introspectApiKey ?? 'test-key',
          introspectPath: config.introspectPath,
          introspectHeader: config.introspectHeader,
          breaker: {
            failureThreshold: config.breaker.failureThreshold,
            resetTimeoutMs: config.breaker.openMs,
          },
          clock: () => clock.now,
          transport,
        });
      } catch (err) {
        // Rule 8 exercised at construction time (contract case
        // `unconfigured-refuses-not-allows`): a verifier that cannot reach
        // its own configuration must refuse, never silently allow.
        assertOutcome(kase.sequence ? kase.sequence[0].expect : kase.expect, {
          status: 500,
          reason: err instanceof Error ? err.message : String(err),
          introspectCalled: false,
          jwksFetched: false,
        });
        return;
      }

      const require: RequireScope = kase.require ?? null;
      const rawToken = await tokenForCase(kase);
      const mutatingMethods = config.mutatingMethods;

      if (kase.sequence) {
        for (const step of kase.sequence as Array<Record<string, any>>) {
          if (step.advanceMs !== undefined) {
            clock.now += step.advanceMs;
            continue;
          }
          currentIntrospectSpec = step.introspect ?? null;
          const repeat = step.repeat ?? 1;
          for (let i = 0; i < repeat; i++) {
            const outcome = await runRequest(
              step.method,
              rawToken,
              require,
              jwtVerifier,
              introspectClient,
              transportSpy,
              jwksSpy,
              mutatingMethods,
            );
            assertOutcome(step.expect, outcome);
          }
        }
        return;
      }

      const outcome = await runRequest(
        kase.method,
        rawToken,
        require,
        jwtVerifier,
        introspectClient,
        transportSpy,
        jwksSpy,
        mutatingMethods,
      );
      assertOutcome(kase.expect, outcome);
    });
  }
});
