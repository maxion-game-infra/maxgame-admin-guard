/**
 * Direct unit tests for `AdminJwtVerifier`'s status classification (contract
 * rule 5, JWKS half): pins each jose error type to 401 or 503 at the module
 * level, independent of `contract.spec.ts`'s end-to-end runner. Every
 * "unavailable JWKS" case here uses a real `jose.createRemoteJWKSet` against
 * a real ephemeral local HTTP server (or a closed port, for the raw
 * transport-failure case) rather than a hand-rolled fake error — see
 * `contract.spec.ts`'s `startJwksServer` doc comment for why.
 */
import { readFileSync } from 'node:fs';
import * as http from 'node:http';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import * as jose from 'jose';
import { afterEach, describe, expect, it } from 'vitest';

import { AdminGuardError } from '../src/core/errors';
import { AdminJwtVerifier } from '../src/core/jwt-verifier';

const dirname = path.dirname(fileURLToPath(import.meta.url));
const CONTRACT_DIR = path.resolve(dirname, '../../contract');

function loadJson(relativePath: string): any {
  return JSON.parse(readFileSync(path.join(CONTRACT_DIR, relativePath), 'utf8'));
}

const jwksDoc = loadJson('fixtures/jwks.json');
const signingPrivateJwk = loadJson('fixtures/signing-key.private.jwk.json');
const wrongPrivateJwk = loadJson('fixtures/wrong-key.private.jwk.json');
const ISSUER = 'admin-auth.maxion.game';

type MintOpts = {
  signWith?: 'wrong-key';
  kid?: string;
  omitKid?: boolean;
  iss?: string;
  expired?: boolean;
};

async function mintToken(opts: MintOpts = {}): Promise<string> {
  const privateJwk = opts.signWith === 'wrong-key' ? wrongPrivateJwk : signingPrivateJwk;
  const privateKey = await jose.importJWK(privateJwk, 'EdDSA');

  const header: jose.JWTHeaderParameters = { alg: 'EdDSA' };
  if (!opts.omitKid) {
    header.kid = opts.kid ?? signingPrivateJwk.kid;
  }

  let signer = new jose.SignJWT({
    sub: '6c7b84de-9e3d-40ef-97eb-6bd47ae170a5',
    role: 'super_admin',
    type: 'admin',
    siteAccess: {},
    tokenVersion: 0,
  })
    .setProtectedHeader(header)
    .setIssuer(opts.iss ?? ISSUER)
    .setIssuedAt();

  signer = opts.expired
    ? signer.setExpirationTime(Math.floor(Date.now() / 1000) - 3600)
    : signer.setExpirationTime('15m');

  return signer.sign(privateKey as Parameters<jose.SignJWT['sign']>[0]);
}

/** Every server this file spins up, closed in `afterEach` even on failure. */
const openServers: http.Server[] = [];

afterEach(async () => {
  await Promise.all(
    openServers.splice(0).map((server) => new Promise<void>((resolve) => server.close(() => resolve()))),
  );
});

type ServeMode =
  | { kind: 'ok' }
  | { kind: 'timeout' }
  | { kind: 'http'; status: number }
  | { kind: 'malformed' };

/** Real ephemeral HTTP server standing in for the IdP's JWKS endpoint. */
async function startJwksServer(mode: ServeMode): Promise<URL> {
  const server = http.createServer((req, res) => {
    req.on('error', () => {});
    if (mode.kind === 'timeout') {
      return; // never respond
    }
    if (mode.kind === 'http') {
      res.writeHead(mode.status, { 'content-type': 'application/json' });
      res.end('{}');
      return;
    }
    if (mode.kind === 'malformed') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end('not valid json{{{');
      return;
    }
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify(jwksDoc));
  });
  server.on('clientError', () => {});
  openServers.push(server);

  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address() as AddressInfo;
  return new URL('/jwks.json', `http://127.0.0.1:${address.port}`);
}

/** A verifier whose JWKS resolver is a genuine `createRemoteJWKSet`, fresh (cold cache) every call. */
function verifierAgainst(url: URL): AdminJwtVerifier {
  const jwks = jose.createRemoteJWKSet(url, { timeoutDuration: 300 });
  return new AdminJwtVerifier({ issuer: ISSUER, jwks });
}

async function expectGuardError(promise: Promise<unknown>): Promise<AdminGuardError> {
  try {
    await promise;
  } catch (err) {
    expect(err).toBeInstanceOf(AdminGuardError);
    return err as AdminGuardError;
  }
  throw new Error('expected AdminJwtVerifier.verify to throw');
}

describe('AdminJwtVerifier status classification', () => {
  describe('JWKS could not be obtained -> 503', () => {
    it('unreachable endpoint (fetch/timeout) is 503, not 401', async () => {
      const url = await startJwksServer({ kind: 'timeout' });
      const err = await expectGuardError(verifierAgainst(url).verify(await mintToken()));
      expect(err.status).toBe(503);
    });

    it('a real transport failure (connection refused) is 503, not 401', async () => {
      // A port nothing is listening on: jose's fetch never even reaches an
      // HTTP response, so the rejection is a raw Node error, not a
      // jose.errors.JOSEError instance at all — the branch distinct from
      // both JWKSTimeout and the bare-JOSEError (non-2xx/malformed) cases.
      const deadPort = await (async () => {
        const server = http.createServer();
        await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
        const { port } = server.address() as AddressInfo;
        await new Promise<void>((resolve) => server.close(() => resolve()));
        return port;
      })();
      const url = new URL('/jwks.json', `http://127.0.0.1:${deadPort}`);
      const err = await expectGuardError(verifierAgainst(url).verify(await mintToken()));
      expect(err.status).toBe(503);
    });

    it('non-2xx from the JWKS endpoint is 503, not 401', async () => {
      const url = await startJwksServer({ kind: 'http', status: 500 });
      const err = await expectGuardError(verifierAgainst(url).verify(await mintToken()));
      expect(err.status).toBe(503);
    });

    it('an unparseable JWKS body is 503, not 401', async () => {
      const url = await startJwksServer({ kind: 'malformed' });
      const err = await expectGuardError(verifierAgainst(url).verify(await mintToken()));
      expect(err.status).toBe(503);
    });
  });

  describe('JWKS fetched fine, token is the problem -> 401', () => {
    it('fetch succeeds but the kid is absent is 401, not 503', async () => {
      const url = await startJwksServer({ kind: 'ok' });
      const token = await mintToken({ kid: 'ed-does-not-exist' });
      const err = await expectGuardError(verifierAgainst(url).verify(token));
      expect(err.status).toBe(401);
    });

    it('expired token is 401 with a distinguishable message', async () => {
      const url = await startJwksServer({ kind: 'ok' });
      const token = await mintToken({ expired: true });
      const err = await expectGuardError(verifierAgainst(url).verify(token));
      expect(err.status).toBe(401);
      expect(err.message).toBe('Token expired');
    });

    it('bad signature is 401', async () => {
      const url = await startJwksServer({ kind: 'ok' });
      const token = await mintToken({ signWith: 'wrong-key' });
      const err = await expectGuardError(verifierAgainst(url).verify(token));
      expect(err.status).toBe(401);
    });

    it('wrong issuer is 401', async () => {
      const url = await startJwksServer({ kind: 'ok' });
      const token = await mintToken({ iss: 'maxion-platform.maxion.game' });
      const err = await expectGuardError(verifierAgainst(url).verify(token));
      expect(err.status).toBe(401);
    });

    it('missing kid never touches the JWKS and is 401', async () => {
      const url = await startJwksServer({ kind: 'ok' });
      const token = await mintToken({ omitKid: true });
      const err = await expectGuardError(verifierAgainst(url).verify(token));
      expect(err.status).toBe(401);
    });

    it('malformed token never touches the JWKS and is 401', async () => {
      const url = await startJwksServer({ kind: 'ok' });
      const err = await expectGuardError(verifierAgainst(url).verify('junk.junk.junk'));
      expect(err.status).toBe(401);
    });
  });
});
