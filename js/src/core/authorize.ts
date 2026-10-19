import {
  adminHasAnySiteAccess,
  adminHasFeatureOnSite,
  adminHasSiteOnToken,
  normalizeAdminSiteAccess,
} from './access';
import { AdminGuardError } from './errors';
import type { AdminIntrospectClient } from './introspect-client';
import type { AdminJwtVerifier } from './jwt-verifier';
import type { AdminAuthContext, AdminSiteAccess, RequireScope } from './types';
import { isActiveIntrospection, DEFAULT_MUTATING_METHODS } from './types';

export type VerifyAdminAuthInput = {
  method: string;
  /** Raw `Authorization` header value, e.g. `"Bearer <jwt>"`. */
  authorizationHeader: string | null | undefined;
  /**
   * Allow an admin with no sites/features (non-super_admin, empty
   * siteAccess) to pass. Defaults to `false` — a zero-access admin is
   * rejected with 403. Mirrors `@AdminAuth({ allowNoAccess: true })`.
   */
  allowNoAccess?: boolean;
  /**
   * The route's site/feature requirement. When given (including `null`,
   * which means "wired without metadata" and refuses), it is checked
   * against the token's signed claims BEFORE introspection and against the
   * live verdict after it. Omit it only if the caller runs
   * {@link checkAdminScope} itself, as `AdminBackOfficeFeatureGuard` does;
   * an omitted scope gets no pre-introspection site/feature refusal.
   */
  require?: RequireScope;
};

export type VerifyAdminAuthDeps = {
  jwtVerifier: AdminJwtVerifier;
  introspectClient: AdminIntrospectClient;
  mutatingMethods?: string[];
};

function extractBearerToken(
  authorizationHeader: string | null | undefined,
): string | null {
  const value = (authorizationHeader ?? '').split('Bearer ')[1];
  return value || null;
}

/**
 * The admin-auth contract's core decision (contract rules 1-6, 8), plus the
 * per-route site/feature check (see {@link checkAdminScope}) when
 * `input.require` is given. A caller that only needs "is this a valid
 * admin" omits `require`; `AdminAuthGuard` + `AdminBackOfficeFeatureGuard`
 * still apply the two halves in sequence.
 *
 * Every request: offline JWKS verification + `type === 'admin'`, then the
 * no-access check and (if `require` is given) the scope check on the
 * token's signed claims. Mutating requests that pass additionally: live
 * introspect, whose verdict REPLACES the token's own (up to
 * token-lifetime-stale) role/siteAccess, and the same checks again on the
 * verdict (contract case `write-live-verdict-overrides-token-grant`). A
 * request the signed claims refuse is never introspected (rule 4, W368
 * option A; contract cases `write-*-without-introspect`).
 */
export async function verifyAdminAuth(
  input: VerifyAdminAuthInput,
  deps: VerifyAdminAuthDeps,
): Promise<AdminAuthContext> {
  const token = extractBearerToken(input.authorizationHeader);
  if (!token) {
    throw new AdminGuardError(401, 'Invalid token');
  }

  const decoded = await deps.jwtVerifier.verify(token);

  if (decoded.type !== 'admin') {
    throw new AdminGuardError(401, 'Invalid token type');
  }

  let role = decoded.role;
  let siteAccess = normalizeAdminSiteAccess(decoded.siteAccess);

  checkAccess(input, decoded.sub, role, siteAccess);

  const mutating = new Set(deps.mutatingMethods ?? DEFAULT_MUTATING_METHODS);
  if (mutating.has(input.method)) {
    const verdict = await deps.introspectClient.introspect(token);
    if (isActiveIntrospection(verdict)) {
      if (!verdict.adminId) {
        // active:true with no adminId names nobody; refuse to authorize a
        // write against an unidentifiable admin.
        throw new AdminGuardError(
          401,
          'Admin session is no longer active (missing adminId)',
        );
      }
      role = verdict.role;
      siteAccess = normalizeAdminSiteAccess(verdict.siteAccess);
    } else {
      throw new AdminGuardError(
        401,
        `Admin session is no longer active (${verdict.reason})`,
      );
    }

    checkAccess(input, decoded.sub, role, siteAccess);
  }

  return { adminId: decoded.sub, adminRole: role, adminSiteAccess: siteAccess };
}

/**
 * Contract rule 6, then (if `input.require` is given) rule 3.
 * {@link verifyAdminAuth} runs it on the signed claims and, on a mutation,
 * again on the live verdict, so both passes refuse with the same status and
 * message.
 */
function checkAccess(
  input: VerifyAdminAuthInput,
  adminId: string,
  role: string,
  siteAccess: AdminSiteAccess,
): void {
  if (!input.allowNoAccess && !adminHasAnySiteAccess(role, siteAccess)) {
    throw new AdminGuardError(
      403,
      'No access assigned. Contact your administrator.',
    );
  }
  if (input.require !== undefined) {
    checkAdminScope(
      { adminId, adminRole: role, adminSiteAccess: siteAccess },
      input.require,
    );
  }
}

/**
 * The per-route site/feature check (contract rule 3 + `missing-scope-metadata-refuses`).
 * A route wired without its site/feature metadata refuses (403) rather than
 * falling through as unguarded.
 */
export function checkAdminScope(
  ctx: AdminAuthContext,
  require: RequireScope,
): void {
  if (!require) {
    throw new AdminGuardError(
      403,
      'Admin back office route is missing scope metadata',
    );
  }

  if ('feature' in require) {
    if (
      !adminHasFeatureOnSite(
        ctx.adminRole,
        ctx.adminSiteAccess,
        require.site,
        require.feature,
      )
    ) {
      throw new AdminGuardError(
        403,
        'Access denied. Required feature permission is missing.',
      );
    }
    return;
  }

  if (!adminHasSiteOnToken(ctx.adminRole, ctx.adminSiteAccess, require.site)) {
    throw new AdminGuardError(
      403,
      'Access denied. This back office site is not allowed for your account.',
    );
  }
}
