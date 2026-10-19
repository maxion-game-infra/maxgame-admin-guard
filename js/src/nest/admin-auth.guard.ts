import type { CanActivate, ExecutionContext } from '@nestjs/common';
import { Inject, Injectable } from '@nestjs/common';
import { Reflector } from '@nestjs/core';

import { verifyAdminAuth } from '../core/authorize';
import { AdminIntrospectClient } from '../core/introspect-client';
import { AdminJwtVerifier } from '../core/jwt-verifier';
import { ADMIN_GUARD_OPTIONS } from './admin-guard.module';
import type { ResolvedAdminGuardNestOptions } from './admin-guard.module';
import { toNestException } from './exceptions';

/**
 * Verifies admin access tokens against the standalone IdP. Every request
 * gets offline JWKS verification (signature/issuer/expiry/`type`) and the
 * no-access check on the token's claims; mutating requests that pass
 * additionally get a live introspect call so a deactivation or a removed
 * grant takes effect on the very next mutation instead of waiting for the
 * access token to expire. A grant ADDED after the token was minted does not
 * reach writes until a refresh (contract rule 4, W368 option A). This guard
 * passes no `require` to {@link verifyAdminAuth}, so a token on the wrong
 * site is still introspected here and refused afterwards by
 * `AdminBackOfficeFeatureGuard`. Sets `req.adminId` / `adminRole` /
 * `adminSiteAccess` for downstream guards/controllers/audit logging.
 */
@Injectable()
export class AdminAuthGuard implements CanActivate {
  constructor(
    private readonly jwtVerifier: AdminJwtVerifier,
    private readonly introspectClient: AdminIntrospectClient,
    private readonly reflector: Reflector,
    @Inject(ADMIN_GUARD_OPTIONS)
    private readonly options: ResolvedAdminGuardNestOptions,
  ) {}

  async canActivate(context: ExecutionContext): Promise<boolean> {
    const request = context.switchToHttp().getRequest();

    const allowNoAccess = this.reflector.getAllAndOverride<boolean>(
      this.options.allowNoAccessMetadataKey,
      [context.getHandler(), context.getClass()],
    );

    try {
      const ctx = await verifyAdminAuth(
        {
          method: request.method,
          authorizationHeader: request.headers?.authorization,
          allowNoAccess,
        },
        {
          jwtVerifier: this.jwtVerifier,
          introspectClient: this.introspectClient,
          mutatingMethods: this.options.mutatingMethods,
        },
      );

      request['adminId'] = ctx.adminId;
      request['adminRole'] = ctx.adminRole;
      request['adminSiteAccess'] = ctx.adminSiteAccess;

      return true;
    } catch (err) {
      throw toNestException(err);
    }
  }
}
