import { checkAdminScope, verifyAdminAuth } from '../core/authorize';
import { AdminGuardError } from '../core/errors';
import { AdminIntrospectClient } from '../core/introspect-client';
import { AdminJwtVerifier } from '../core/jwt-verifier';
import type { AdminAuthContext, AdminGuardConfig, RequireScope } from '../core/types';

/**
 * Minimal shape this module needs from a request — deliberately not tied to
 * Express/Connect/Fastify/the raw Node `http` module, so it works with
 * whatever a plain Node router (e.g. email-server) hands it.
 */
export type AdminGuardRequest = {
  method?: string;
  headers: Record<string, string | string[] | undefined>;
  [key: string]: unknown;
};

export type AdminGuardResponse = {
  statusCode?: number;
  setHeader(name: string, value: string): unknown;
  end(chunk?: string): unknown;
};

export type AdminGuardNext = (err?: unknown) => void;

function headerValue(
  value: string | string[] | undefined,
): string | undefined {
  return Array.isArray(value) ? value[0] : value;
}

/**
 * Runs the admin-auth contract (auth + optional scope check) against a bare
 * request object and either returns the context or throws
 * {@link AdminGuardError}. Framework-free — use this directly if you don't
 * want the `(req, res, next)` convenience wrapper below (e.g. to control
 * the error response shape yourself).
 */
export async function authorizeAdminRequest(
  req: AdminGuardRequest,
  deps: {
    jwtVerifier: AdminJwtVerifier;
    introspectClient: AdminIntrospectClient;
    mutatingMethods?: string[];
  },
  options: { require?: RequireScope; allowNoAccess?: boolean } = {},
): Promise<AdminAuthContext> {
  const ctx = await verifyAdminAuth(
    {
      method: req.method ?? 'GET',
      authorizationHeader: headerValue(req.headers['authorization']),
      allowNoAccess: options.allowNoAccess,
    },
    deps,
  );
  if (options.require !== undefined) {
    checkAdminScope(ctx, options.require);
  }
  return ctx;
}

export type AdminAuthMiddlewareOptions = {
  require?: RequireScope;
  allowNoAccess?: boolean;
};

/**
 * Builds a `(req, res, next)` middleware for a plain Node router. On
 * success, sets `req.adminId` / `req.adminRole` / `req.adminSiteAccess` and
 * calls `next()`; on failure, writes a JSON error body with the status the
 * contract requires (401/403/503) and does NOT call `next()`.
 *
 * Constructs its own `AdminJwtVerifier` / `AdminIntrospectClient` from
 * `config` — call this once per required scope at startup (mirrors
 * `AdminGuardModule.forRoot` for the Nest side), not per request.
 */
export function createAdminAuthMiddleware(
  config: AdminGuardConfig,
  options: AdminAuthMiddlewareOptions = {},
) {
  const jwtVerifier = new AdminJwtVerifier({
    issuer: config.issuer,
    idpBaseUrl: config.idpBaseUrl,
    jwksPath: config.jwksPath,
  });
  const introspectClient = new AdminIntrospectClient({
    idpBaseUrl: config.idpBaseUrl,
    introspectApiKey: config.introspectApiKey,
    introspectPath: config.introspectPath,
    introspectHeader: config.introspectHeader,
    timeoutMs: config.introspectTimeoutMs,
    breaker: config.breaker,
    clock: config.clock,
  });

  return async function adminAuthMiddleware(
    req: AdminGuardRequest,
    res: AdminGuardResponse,
    next: AdminGuardNext,
  ): Promise<void> {
    try {
      const ctx = await authorizeAdminRequest(
        req,
        { jwtVerifier, introspectClient, mutatingMethods: config.mutatingMethods },
        options,
      );
      req['adminId'] = ctx.adminId;
      req['adminRole'] = ctx.adminRole;
      req['adminSiteAccess'] = ctx.adminSiteAccess;
      next();
    } catch (err) {
      sendAdminGuardError(res, err);
    }
  };
}

function sendAdminGuardError(res: AdminGuardResponse, err: unknown): void {
  const status = err instanceof AdminGuardError ? err.status : 500;
  const message = err instanceof Error ? err.message : 'Internal error';
  res.statusCode = status;
  res.setHeader('content-type', 'application/json');
  res.end(JSON.stringify({ error: message }));
}
