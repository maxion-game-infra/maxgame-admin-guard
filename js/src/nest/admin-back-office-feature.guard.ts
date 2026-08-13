import type { CanActivate, ExecutionContext } from '@nestjs/common';
import { Inject, Injectable } from '@nestjs/common';
import { Reflector } from '@nestjs/core';

import { checkAdminScope } from '../core/authorize';
import type { AdminAuthContext, RequireScope } from '../core/types';
import { ADMIN_GUARD_OPTIONS } from './admin-guard.module';
import type { ResolvedAdminGuardNestOptions } from './admin-guard.module';
import { toNestException } from './exceptions';

/**
 * Site/feature authorization, run after {@link AdminAuthGuard} has already
 * populated `req.adminRole` / `req.adminSiteAccess`. Reads its scope
 * requirement from route metadata (site-only, or site+feature) — a route
 * wired without that metadata refuses (403) rather than falling through as
 * unguarded.
 */
@Injectable()
export class AdminBackOfficeFeatureGuard implements CanActivate {
  constructor(
    private readonly reflector: Reflector,
    @Inject(ADMIN_GUARD_OPTIONS)
    private readonly options: ResolvedAdminGuardNestOptions,
  ) {}

  canActivate(context: ExecutionContext): boolean {
    const require = this.reflector.getAllAndOverride<RequireScope>(
      this.options.scopeMetadataKey,
      [context.getHandler(), context.getClass()],
    );

    const request = context.switchToHttp().getRequest();
    const ctx: AdminAuthContext = {
      adminId: request['adminId'],
      adminRole: request['adminRole'],
      adminSiteAccess: request['adminSiteAccess'],
    };

    try {
      checkAdminScope(ctx, require);
      return true;
    } catch (err) {
      throw toNestException(err);
    }
  }
}
