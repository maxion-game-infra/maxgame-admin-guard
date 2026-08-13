/**
 * Drives every case in `../../contract/cases.json` through the real core
 * verifier (`AdminJwtVerifier` + `AdminIntrospectClient` + `verifyAdminAuth`
 * + `checkAdminScope`) — not a reimplementation of the rules, the actual
 * production code path, with introspection stubbed per case and a fake
 * clock for the breaker's `sequence` cases. See contract/README.md.
 */
import { readFileSync } from 'node:fs';
import * as http from 'node:http';
import type { AddressInfo } from 'node:net';
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

/**
 * What a case can stub the JWKS endpoint to. `coldCache: true` is required
 * on every one of these — the point is to exercise a genuine fetch, not a
 * warm/cached resolver — and is asserted below.
 */
type JwksSpec = {
  coldCache: true;
  transport?: 'timeout' | 'http' | 'malformed';
  status?: number;
};

/**
 * Cases with a `jwks` field get a *real* `jose.createRemoteJWKSet` pointed
 * at a real (ephemeral, localhost-only) HTTP server, rather than a
 * hand-rolled resolver that throws a made-up error. jose's own fetch path
 * throws specific, version-pinned error types depending on exactly how the
 * fetch failed (`JWKSTimeout` for a timeout, its bare `JOSEError` base
 * class for a non-2xx or unparseable body) — faking those by hand risks
 * testing a shape the fix's classifier was never actually written against.
 * A fresh server + a fresh `createRemoteJWKSet` per case also guarantees
 * "cold cache": there is no cache to be warm.
 */
async function startJwksServer(spec: JwksSpec): Promise<{ url: URL; close: () => Promise<void> }> {
  const server = http.createServer((req, res) => {
    req.on('error', () => {
      // The client destroys the socket once its own timeout fires; that
      // must not surface as an unhandled error on the server side.
    });
    if (spec.transport === 'timeout') {
      // Never respond. The client's own timeoutDuration cuts this short.
      return;
    }
    if (spec.transport === 'http') {
      res.writeHead(spec.status ?? 500, { 'content-type': 'application/json' });
      res.end('{}');
      return;
    }
    if (spec.transport === 'malformed') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end('not valid json{{{');
      return;
    }
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify(jwksDoc));
  });
  server.on('clientError', () => {
    // Belt-and-suspenders: a socket the client tore down mid-request must
    // not throw an unhandled error and fail the test.
  });

  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address() as AddressInfo;
  return {
    url: new URL('/jwks.json', `http://127.0.0.1:${address.port}`),
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

/**
 * Builds the jwks resolver for one case: the real remote fetch (via
 * {@link startJwksServer}) when the case carries a `jwks` field, otherwise
 * the existing zero-network local resolver every other case already uses.
 * Returns a teardown to close the server, a no-op for the local-resolver
 * path.
 */
async function buildJwksResolverForCase(
  kaseJwks: JwksSpec | undefined,
  jwksSpy: ReturnType<typeof vi.fn>,
): Promise<{ resolver: JwksResolver; teardown: () => Promise<void> }> {
  if (!kaseJwks) {
    type LocalJwksFn = ReturnType<typeof jose.createLocalJWKSet>;
    const baseResolver: LocalJwksFn = jose.createLocalJWKSet(jwksDoc);
    const resolver = (async (...args: Parameters<LocalJwksFn>) => {
      jwksSpy();
      return baseResolver(...args);
    }) as JwksResolver;
    return { resolver, teardown: async () => {} };
  }

  if (!kaseJwks.coldCache) {
    throw new Error('contract test bug: every jwks case must set coldCache: true');
  }

  const { url, close } = await startJwksServer(kaseJwks);
  // Short timeoutDuration so the `timeout` case resolves in well under a
  // second instead of jose's 5s default; harmless for the other transports
  // since the local server answers immediately.
  const remote = jose.createRemoteJWKSet(url, { timeoutDuration: 300 });
  const resolver = (async (...args: Parameters<typeof remote>) => {
    jwksSpy();
    return remote(...args);
  }) as JwksResolver;
  return { resolver, teardown: close };
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
      const { resolver: spiedJwks, teardown: teardownJwks } = await buildJwksResolverForCase(
        kase.jwks as JwksSpec | undefined,
        jwksSpy,
      );

      let currentIntrospectSpec: IntrospectSpec = kase.introspect ?? null;
      const transportSpy = vi.fn();
      const transport: IntrospectTransport = async (token: string) => {
        transportSpy(token);
        return specToTransportResult(currentIntrospectSpec);
      };

      try {
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
      } finally {
        await teardownJwks();
      }
    });
  }
});
